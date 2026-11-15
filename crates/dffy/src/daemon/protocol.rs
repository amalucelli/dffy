//! What dffy and dffyd say to each other over the socket. A frame is a
//! little-endian `u32` body length, then the body: a tag byte and the
//! message's fields. Integers are little-endian, byte strings and paths are
//! a `u32` length and the bytes.
//!
//! The socket's name carries [`VERSION`], so a dffy only ever talks to a
//! broker of its own protocol version; brokers of other versions run side by
//! side. Change the version with any change to this format.
//!
//! A connection does one thing: `Subscribe` (the broker answers `Ready`,
//! then streams `Changed`, `Rescan` and `Polling` until either side hangs
//! up), `Status` or `Describe` (answered by `Report`) or `Stop` (answered
//! by `Stopping`). `Error` answers a request the broker can't serve.
//!
//! `Describe` came after the first brokers of this version, which hang up
//! on it: it's `Status` with the broker's build in the answer, in a frame
//! of its own so that `Status` answers stay as older clients read them.

use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

pub(crate) const VERSION: u32 = 1;

/// Frames above this are refused rather than allocated: a batch is at most
/// 4,096 paths.
const MAX_FRAME: u32 = 64 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Message {
    /// Watch the repository containing this path.
    Subscribe(PathBuf),
    Status,
    /// `Status`, answered with the broker's build too.
    Describe,
    Stop,
    /// The subscription is live: changes from now on are reported.
    Ready {
        backend: String,
        watches: u32,
    },
    Changed(Vec<PathBuf>),
    Rescan,
    Polling(String),
    Report(Report),
    Stopping {
        pid: u32,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Report {
    pub pid: u32,
    pub uptime_secs: u64,
    pub roots: Vec<RootReport>,
    /// The broker's [`crate::BUILD`], in answers to `Describe`.
    pub build: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootReport {
    pub path: PathBuf,
    pub subscribers: u32,
    pub watches: u32,
    pub backend: String,
    /// Why the root is polled instead of watched.
    pub polling: Option<String>,
}

impl Message {
    /// The whole frame, length included.
    #[must_use]
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut w = Writer(vec![0; 4]);
        match self {
            Self::Subscribe(path) => {
                w.u8(1);
                w.bytes(path.as_os_str().as_bytes());
            }
            Self::Status => w.u8(2),
            Self::Stop => w.u8(3),
            Self::Describe => w.u8(4),
            Self::Ready { backend, watches } => {
                w.u8(64);
                w.bytes(backend.as_bytes());
                w.u32(*watches);
            }
            Self::Changed(paths) => {
                w.u8(65);
                w.len(paths.len());
                for path in paths {
                    w.bytes(path.as_os_str().as_bytes());
                }
            }
            Self::Rescan => w.u8(66),
            Self::Polling(reason) => {
                w.u8(67);
                w.bytes(reason.as_bytes());
            }
            Self::Report(report) => {
                w.u8(if report.build.is_some() { 71 } else { 68 });
                w.u32(report.pid);
                w.u64(report.uptime_secs);
                w.len(report.roots.len());
                for root in &report.roots {
                    w.bytes(root.path.as_os_str().as_bytes());
                    w.u32(root.subscribers);
                    w.u32(root.watches);
                    w.bytes(root.backend.as_bytes());
                    w.bytes(root.polling.as_deref().unwrap_or_default().as_bytes());
                }
                if let Some(build) = &report.build {
                    w.bytes(build.as_bytes());
                }
            }
            Self::Stopping { pid } => {
                w.u8(69);
                w.u32(*pid);
            }
            Self::Error(message) => {
                w.u8(70);
                w.bytes(message.as_bytes());
            }
        }
        let mut frame = w.0;
        let len = u32::try_from(frame.len() - 4).unwrap_or(u32::MAX);
        frame[..4].copy_from_slice(&len.to_le_bytes());
        frame
    }

    /// Reads one frame.
    ///
    /// # Errors
    ///
    /// Read errors, `UnexpectedEof` when the peer hung up, `InvalidData` for
    /// a malformed or oversized frame.
    pub(crate) fn read(from: &mut impl Read) -> io::Result<Self> {
        let mut len = [0; 4];
        from.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len);
        if len > MAX_FRAME {
            return Err(invalid());
        }
        let mut body = vec![0; len as usize];
        from.read_exact(&mut body)?;
        let mut r = Reader(&body);
        let message = match r.u8()? {
            1 => Self::Subscribe(r.path()?),
            2 => Self::Status,
            3 => Self::Stop,
            4 => Self::Describe,
            64 => Self::Ready {
                backend: r.string()?,
                watches: r.u32()?,
            },
            65 => {
                let n = r.u32()?;
                Self::Changed((0..n).map(|_| r.path()).collect::<io::Result<_>>()?)
            }
            66 => Self::Rescan,
            67 => Self::Polling(r.string()?),
            tag @ (68 | 71) => {
                let (pid, uptime_secs, n) = (r.u32()?, r.u64()?, r.u32()?);
                let roots = (0..n)
                    .map(|_| {
                        Ok(RootReport {
                            path: r.path()?,
                            subscribers: r.u32()?,
                            watches: r.u32()?,
                            backend: r.string()?,
                            polling: Some(r.string()?).filter(|s| !s.is_empty()),
                        })
                    })
                    .collect::<io::Result<_>>()?;
                let build = if tag == 71 { Some(r.string()?) } else { None };
                Self::Report(Report {
                    pid,
                    uptime_secs,
                    roots,
                    build,
                })
            }
            69 => Self::Stopping { pid: r.u32()? },
            70 => Self::Error(r.string()?),
            _ => return Err(invalid()),
        };
        if r.0.is_empty() {
            Ok(message)
        } else {
            Err(invalid())
        }
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed dffyd message")
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }

    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.extend_from_slice(b);
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        let (head, rest) = self.0.split_at_checked(n).ok_or_else(invalid)?;
        self.0 = rest;
        Ok(head)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> io::Result<u32> {
        let mut b = [0; 4];
        b.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> io::Result<u64> {
        let mut b = [0; 8];
        b.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(b))
    }

    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    fn path(&mut self) -> io::Result<PathBuf> {
        Ok(PathBuf::from(std::ffi::OsString::from_vec(self.bytes()?)))
    }

    fn string(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| invalid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_survives_a_roundtrip() {
        let report = Report {
            pid: 4242,
            uptime_secs: 3_600,
            roots: vec![
                RootReport {
                    path: "/src/linux".into(),
                    subscribers: 3,
                    watches: 6_341,
                    backend: "inotify".into(),
                    polling: None,
                },
                RootReport {
                    path: "/src/other".into(),
                    subscribers: 1,
                    watches: 0,
                    backend: "polling".into(),
                    polling: Some("inotify watch limit reached".into()),
                },
            ],
            build: None,
        };
        let described = Report {
            build: Some("0.1.0+0123abcd".into()),
            ..report.clone()
        };
        let messages = [
            Message::Subscribe("/src/linux".into()),
            Message::Status,
            Message::Describe,
            Message::Stop,
            Message::Ready {
                backend: "inotify".into(),
                watches: 6_341,
            },
            Message::Changed(vec![
                "/src/a b".into(),
                PathBuf::from(std::ffi::OsString::from_vec(b"\xff\n".to_vec())),
            ]),
            Message::Changed(Vec::new()),
            Message::Rescan,
            Message::Polling("limit".into()),
            Message::Report(report),
            Message::Report(described),
            Message::Stopping { pid: 7 },
            Message::Error("not a git repository".into()),
        ];
        let stream: Vec<u8> = messages.iter().flat_map(Message::encode).collect();
        let mut from = &stream[..];
        for message in &messages {
            assert_eq!(&Message::read(&mut from).expect("decodes"), message);
        }
        assert_eq!(from, []);
    }

    /// A `Status` answer is byte for byte what the first clients of this
    /// version read, since they refuse anything after the fields they know.
    #[test]
    fn a_status_report_keeps_its_first_frame() {
        let report = Message::Report(Report {
            pid: 7,
            uptime_secs: 9,
            roots: vec![RootReport {
                path: "/r".into(),
                subscribers: 1,
                watches: 2,
                backend: "inotify".into(),
                polling: None,
            }],
            build: None,
        });
        let mut expected = vec![
            46, 0, 0, 0, 68, 7, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0,
        ];
        expected.extend([2, 0, 0, 0, b'/', b'r', 1, 0, 0, 0, 2, 0, 0, 0, 7, 0, 0, 0]);
        expected.extend(b"inotify");
        expected.extend([0, 0, 0, 0]);
        assert_eq!(report.encode(), expected);
    }

    #[test]
    fn malformed_frames_are_refused() {
        let mut frame = Message::Ready {
            backend: "inotify".into(),
            watches: 1,
        }
        .encode();
        frame.push(0);
        frame[0] += 1;
        assert_eq!(
            Message::read(&mut &frame[..]).map_err(|e| e.kind()),
            Err(io::ErrorKind::InvalidData)
        );
        let huge = (MAX_FRAME + 1).to_le_bytes();
        assert!(Message::read(&mut &huge[..]).is_err());
        assert_eq!(
            Message::read(&mut &b"\x05\x00"[..]).map_err(|e| e.kind()),
            Err(io::ErrorKind::UnexpectedEof)
        );
    }
}
