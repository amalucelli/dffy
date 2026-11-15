//! The terminal behind the TUI: raw mode and the alternate screen, the
//! input thread, signals, and putting the terminal back on every way out:
//! a normal exit, a panic, and while a child process (the editor) has it.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::termios::{self, OptionalActions, Termios};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
use signal_hook::iterator::Signals;

use super::input::{ESC_TIMEOUT_MS, Input, Parser};

/// Alternate screen, hidden cursor, no autowrap (so writing the
/// bottom-right cell never scrolls), and button, wheel and drag reports
/// (motion only while a button is held) in the SGR mouse encoding.
pub(super) const ENTER: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub(super) const LEAVE: &[u8] =
    b"\x1b[0m\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?7h\x1b[?25h\x1b[?1049l";
/// Mouse reports off, so the terminal's own selection works, and back on.
pub(crate) const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1002l\x1b[?1000l";
pub(crate) const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";

/// What [`restore`] puts back while the TUI owns the terminal. Global so the
/// panic hook reaches it: release builds abort on panic, skipping
/// destructors.
static OWNED: Mutex<Option<Saved>> = Mutex::new(None);

struct Saved {
    fd: OwnedFd,
    termios: Termios,
}

pub(crate) struct Tty {
    file: File,
    termios: Termios,
}

impl Tty {
    /// Opens the terminal stdout is on, falling back to `/dev/tty`. The
    /// device's own path comes first because macOS can't poll `/dev/tty`.
    pub(crate) fn open() -> io::Result<Self> {
        let open = |path: PathBuf| OpenOptions::new().read(true).write(true).open(path);
        let file = termios::ttyname(io::stdout(), Vec::new())
            .ok()
            .and_then(|name| open(OsString::from_vec(name.into_bytes()).into()).ok())
            .map_or_else(|| open("/dev/tty".into()), Ok)?;
        let termios = termios::tcgetattr(&file)?;
        Ok(Self { file, termios })
    }

    /// Columns and rows, at least 1 each.
    pub(crate) fn size(&self) -> (usize, usize) {
        termios::tcgetwinsize(&self.file).map_or((80, 24), |s| {
            (usize::from(s.ws_col.max(1)), usize::from(s.ws_row.max(1)))
        })
    }

    /// Raw mode and the alternate screen, until [`restore`].
    pub(crate) fn enter(&self) -> io::Result<()> {
        let mut raw = self.termios.clone();
        raw.make_raw();
        *OWNED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Saved {
            fd: self.file.as_fd().try_clone_to_owned()?,
            termios: self.termios.clone(),
        });
        termios::tcsetattr(&self.file, OptionalActions::Now, &raw)?;
        self.write(ENTER)
    }

    pub(crate) fn write(&self, bytes: &[u8]) -> io::Result<()> {
        (&self.file).write_all(bytes)
    }

    /// A handle for child processes that need the terminal as stdin.
    pub(crate) fn try_clone(&self) -> io::Result<File> {
        self.file.try_clone()
    }

    /// Starts the input thread. Parsed input goes to `send` until it returns
    /// false; `None` means the terminal is gone.
    pub(crate) fn spawn_reader(
        &self,
        mut send: impl FnMut(Option<Input>) -> bool + Send + 'static,
    ) -> io::Result<Reader> {
        let tty = self.file.try_clone()?;
        let (wake_rx, wake) = rustix::pipe::pipe()?;
        // Not inherited by dffyd, which a watching viewer may start. macOS
        // has no pipe2, hence the separate call.
        for fd in [&wake_rx, &wake] {
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
        }
        let (control, control_rx) = mpsc::channel();
        let (ack_tx, ack) = mpsc::channel();
        thread::Builder::new()
            .name("dffy-input".into())
            .spawn(move || {
                let mut parser = Parser::default();
                let mut buf = [0u8; 4096];
                let timeout = Timespec {
                    tv_sec: 0,
                    tv_nsec: i64::try_from(ESC_TIMEOUT_MS * 1_000_000).unwrap_or(25_000_000),
                };
                let mut parsed = Vec::new();
                loop {
                    for input in parsed.drain(..) {
                        if !send(Some(input)) {
                            return;
                        }
                    }
                    let mut emit = |input| parsed.push(input);
                    let mut fds = [
                        PollFd::new(&tty, PollFlags::IN),
                        PollFd::new(&wake_rx, PollFlags::IN),
                    ];
                    let ready = match poll(&mut fds, parser.pending().then_some(&timeout)) {
                        Ok(n) => n,
                        Err(rustix::io::Errno::INTR) => continue,
                        Err(_) => 0,
                    };
                    let (input, woken) = (fds[0].revents(), fds[1].revents());
                    if ready == 0 {
                        parser.timeout(&mut emit);
                    } else if !woken.is_empty() {
                        let _ = rustix::io::read(&wake_rx, &mut buf);
                        while let Ok(pause) = control_rx.try_recv() {
                            if pause {
                                parser = Parser::default();
                                let _ = ack_tx.send(());
                                // Someone else reads the terminal until resumed.
                                if control_rx.recv().is_err() {
                                    return;
                                }
                            }
                        }
                    } else if !input.is_empty() {
                        match (&tty).read(&mut buf) {
                            Ok(0) => {
                                send(None);
                                return;
                            }
                            Ok(n) => parser.feed(&buf[..n], &mut emit),
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(_) => {
                                send(None);
                                return;
                            }
                        }
                    }
                }
            })?;
        Ok(Reader { wake, control, ack })
    }
}

/// The input thread's controls.
pub(crate) struct Reader {
    wake: OwnedFd,
    control: mpsc::Sender<bool>,
    ack: mpsc::Receiver<()>,
}

impl Reader {
    /// Stops reading the terminal, returning once the thread has stopped, so
    /// a child process gets every key.
    pub(crate) fn pause(&self) {
        if self.control.send(true).is_ok() {
            let _ = rustix::io::write(&self.wake, b"p");
            let _ = self.ack.recv_timeout(Duration::from_secs(1));
        }
    }

    pub(crate) fn resume(&self) {
        let _ = self.control.send(false);
    }
}

/// Puts the terminal back as [`Tty::enter`] found it. Does nothing when the
/// TUI doesn't own the terminal, so every way out can call it.
pub(crate) fn restore() {
    let saved = OWNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(saved) = saved {
        let _ = rustix::io::write(&saved.fd, LEAVE);
        // Flush drops input still queued, such as mouse reports sent before
        // reporting was turned off, so it doesn't land in the shell.
        let _ = termios::tcsetattr(&saved.fd, OptionalActions::Flush, &saved.termios);
    }
}

/// Restores the terminal before the default hook prints the panic.
pub(crate) fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    Resize,
    /// SIGINT, SIGTERM or SIGHUP.
    Interrupt,
}

/// Delivers signals to `f` on a thread of their own until it returns false.
/// While registered, SIGINT, SIGTERM and SIGHUP no longer kill the process.
pub(crate) fn on_signals(mut f: impl FnMut(Signal) -> bool + Send + 'static) -> io::Result<()> {
    let mut signals = Signals::new([SIGWINCH, SIGINT, SIGTERM, SIGHUP])?;
    thread::Builder::new()
        .name("dffy-signals".into())
        .spawn(move || {
            for signal in signals.forever() {
                let signal = if signal == SIGWINCH {
                    Signal::Resize
                } else {
                    Signal::Interrupt
                };
                if !f(signal) {
                    break;
                }
            }
        })?;
    Ok(())
}
