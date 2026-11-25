//! dffyd, the per-user watch broker, and the client side of it. dffyd also
//! serves the control API ([`crate::control`]) on a socket of its own.
//!
//! The Linux inotify limit is per user and costs one watch per directory, so
//! every dffy view of a repository watching it alone would multiply the
//! cost: ten kernel trees with three views each need 190k watches against a
//! default of about 130k on 16GB. The broker holds one watcher per
//! repository and fans its changes out to every view. Nothing else is
//! shared: each view still runs its own engine, and print and pager mode
//! never touch the broker.
//!
//! Lifecycle: the first viewer starts dffyd, to register with the control
//! API or to watch, or the first listening session does, and it exits a
//! minute after its last subscriber, view and session leave. When it can't be reached, or dies, a view watches by itself.
//! Its sockets live in a private directory (`$XDG_RUNTIME_DIR/dffy`, else
//! `$TMPDIR/dffy-<uid>`) and are named after their protocol's version, so
//! dffyds from different dffy versions never meet; each socket is served
//! by whichever dffyd holds its lock.

pub(crate) mod broker;
pub(crate) mod client;
pub(crate) mod protocol;

use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::{Message, Report, VERSION};

use crate::render::{Canvas, escape};
use crate::theme::Role;

/// The first argument that makes `dffy` start dffyd (see [`client::start`]).
pub(crate) const START: &str = "__dffyd";

/// The broker's private directory, created if needed. Refuses one that
/// isn't a directory owned by this user and closed to everyone else, since
/// anyone who can write there could stand in for the broker.
///
/// # Errors
///
/// When the directory can't be created or isn't private.
pub(crate) fn runtime_dir() -> io::Result<PathBuf> {
    let dir = runtime_path();
    if let Err(e) = DirBuilder::new().mode(0o700).create(&dir)
        && e.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(e);
    }
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir()
        || meta.uid() != rustix::process::getuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        return Err(io::Error::other(format!(
            "{} isn't a private directory of yours",
            dir.display()
        )));
    }
    Ok(dir)
}

/// Where [`runtime_dir`] is, without creating it.
pub(crate) fn runtime_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| Path::new(v).is_absolute()) {
        Some(base) => PathBuf::from(base).join("dffy"),
        None => std::env::temp_dir().join(format!("dffy-{}", rustix::process::getuid().as_raw())),
    }
}

fn socket_path(dir: &Path) -> PathBuf {
    dir.join(format!("broker-v{VERSION}.sock"))
}

/// Connects to one of dffyd's sockets, `Err` when nothing serves it. The
/// socket must be this user's.
pub(crate) fn connect(path: &Path) -> io::Result<UnixStream> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_socket() || meta.uid() != rustix::process::getuid().as_raw() {
        return Err(io::Error::other(format!(
            "{} isn't a socket of yours",
            path.display()
        )));
    }
    UnixStream::connect(path)
}

/// Sends a one-shot request to the running broker; `None` when none runs.
pub(crate) fn request(message: &Message) -> io::Result<Option<Message>> {
    let Ok(mut stream) = connect(&socket_path(&runtime_path())) else {
        return Ok(None);
    };
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let answer = stream
        .write_all(&message.encode())
        .and_then(|()| Message::read(&mut stream));
    match answer {
        Ok(answer) => Ok(Some(answer)),
        // It went away meanwhile: exiting, idle or killed.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::UnexpectedEof
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// The running broker's report, `None` when none runs. A broker older than
/// `Describe` hangs up on it and is asked for its `Status`, which comes
/// without a build.
pub(crate) fn report() -> io::Result<Option<Report>> {
    let answer = match request(&Message::Describe)? {
        Some(answer) => Some(answer),
        None => request(&Message::Status)?,
    };
    match answer {
        Some(Message::Report(report)) => Ok(Some(report)),
        Some(other) => Err(unexpected(&other)),
        None => Ok(None),
    }
}

/// The protocol versions of brokers from other dffy versions that answer
/// on their sockets in the runtime directory.
pub(crate) fn other_brokers() -> Vec<u32> {
    let mut versions: Vec<u32> = fs::read_dir(runtime_path())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let version = name
                .to_str()?
                .strip_prefix("broker-v")?
                .strip_suffix(".sock")?
                .parse()
                .ok()?;
            (version != VERSION && UnixStream::connect(entry.path()).is_ok()).then_some(version)
        })
        .collect();
    versions.sort_unstable();
    versions
}

/// `dffy daemon status`.
pub(crate) fn status(c: &mut Canvas) -> io::Result<()> {
    match report()? {
        Some(report) => render(&report, c),
        None => not_running(c),
    }
    Ok(())
}

/// `dffy daemon stop`.
pub(crate) fn stop(c: &mut Canvas) -> io::Result<()> {
    match request(&Message::Stop)? {
        Some(Message::Stopping { pid }) => {
            c.push(Role::Ok, "stopped");
            c.line(Role::Plain, &format!(" dffyd (pid {pid})"));
        }
        Some(other) => return Err(unexpected(&other)),
        None => not_running(c),
    }
    Ok(())
}

fn unexpected(message: &Message) -> io::Error {
    io::Error::other(format!("dffyd answered with {message:?}"))
}

fn not_running(c: &mut Canvas) {
    c.push(Role::Plain, "dffyd");
    c.push(Role::Dim, " isn't running");
    c.line(
        Role::Dim,
        "; the first viewer or listening session starts it",
    );
}

fn render(report: &Report, c: &mut Canvas) {
    let path = |p: &Path| escape(p.as_os_str().as_bytes());
    let text = |s: &str| escape(s.as_bytes());
    c.push(Role::Plain, "dffyd ");
    c.push(Role::Ok, "running");
    c.line(
        Role::Dim,
        &format!(
            "  pid {}, up {}, protocol v{VERSION}",
            report.pid,
            uptime(report.uptime_secs)
        ),
    );
    c.push(Role::Dim, "build ");
    match &report.build {
        Some(build) if build == crate::BUILD => c.line(Role::Plain, build),
        Some(build) => {
            c.push(Role::Plain, &text(build));
            c.line(Role::Warn, &format!(" (this dffy is {})", crate::BUILD));
        }
        None => c.line(Role::Warn, "unknown: the broker predates build reports"),
    }
    c.push(Role::Dim, "socket ");
    c.line(Role::Plain, &path(&socket_path(&runtime_path())));
    let roots = report.roots.len();
    c.line(
        Role::Dim,
        &format!("{roots} root{}", if roots == 1 { "" } else { "s" }),
    );
    for root in &report.roots {
        let subscribers = root.subscribers;
        c.push(Role::Plain, "  ");
        c.push(Role::FileHeader, &path(&root.path));
        c.push(
            Role::Plain,
            &format!(
                "  {subscribers} subscriber{}",
                if subscribers == 1 { "" } else { "s" }
            ),
        );
        match &root.polling {
            Some(reason) => {
                c.push(Role::Warn, "  polling");
                c.line(Role::Dim, &format!(": {}", text(reason)));
            }
            None => c.line(
                Role::Dim,
                &format!("  {} watches  {}", root.watches, text(&root.backend)),
            ),
        }
    }
}

fn uptime(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
}

/// `dffy daemon watch`, a hidden debugging aid: subscribes like a TUI would
/// and prints one line per event until killed. `changed` lines carry one
/// path each.
pub(crate) fn watch(path: &Path) -> Result<(), crate::Error> {
    let root = dffy_core::git::Repo::discover(path)?.watch_root()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let _subscription = client::subscribe(root, move |event| tx.send(event).is_ok())?;
    let mut out = io::stdout().lock();
    for event in rx {
        match event {
            client::Event::Ready { backend, watches } => writeln!(out, "ready {backend} {watches}"),
            client::Event::Local(reason) => writeln!(out, "local {reason}"),
            client::Event::Notice(dffy_core::watch::Notice::Changed(paths)) => paths
                .iter()
                .try_for_each(|p| writeln!(out, "changed {}", p.display())),
            client::Event::Notice(dffy_core::watch::Notice::Rescan) => writeln!(out, "rescan"),
            client::Event::Notice(dffy_core::watch::Notice::Polling(reason)) => {
                writeln!(out, "polling {reason}")
            }
        }?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::RootReport;

    /// A repository whose path holds control bytes can't drive the terminal
    /// through `dffy daemon status`.
    #[test]
    fn status_shows_control_bytes_in_paths_escaped() {
        let report = Report {
            roots: vec![RootReport {
                path: PathBuf::from(std::ffi::OsStr::from_bytes(b"/r/\x1b]0;x\x07")),
                subscribers: 1,
                watches: 2,
                backend: "inotify".to_owned(),
                polling: Some("limit\x1b[2J".to_owned()),
            }],
            build: Some("b\x1b[2J".to_owned()),
            ..Report::default()
        };
        let mut c = Canvas::default();
        render(&report, &mut c);
        let mut out = Vec::new();
        c.write_to(&mut out, None).expect("write");
        let text = String::from_utf8(out).expect("utf-8");
        assert!(!text.contains(['\x1b', '\x07']), "{text:?}");
        assert!(text.contains("/r/^[]0;x^G"), "{text}");
    }
}
