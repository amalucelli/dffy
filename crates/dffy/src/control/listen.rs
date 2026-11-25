//! `dffy comments listen`, a coding agent's session waiting for what viewers
//! hand over, and `dffy comments listening`, what the session says it did.
//!
//! An agent's monitor runs `listen` and takes its exit, whatever the status,
//! for something to react to. So it only ends with its session, and waits
//! out everything else: no repository, no dffyd, one too old to know it.

use std::collections::HashSet;
use std::io::{self, Write as _};
use std::ops::Range;
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use super::client::{self, Client};
use super::{object, text};
use crate::Error;
use crate::plugin::json::Value;

/// The wait before connecting again, doubling to the end while connections
/// don't last.
pub(super) const BACKOFF: Range<Duration> = Duration::from_millis(100)..Duration::from_secs(30);
/// How long [`listening`] waits for dffyd: an agent waits for its hooks.
const REPORT: Duration = Duration::from_secs(1);

/// `dffy comments listen`: prints each prompt handed over in `dir`'s working
/// tree as a line.
///
/// # Errors
///
/// When stdout can't be written.
pub(crate) fn listen(dir: Option<&Path>, name: &str, key: Option<&str>) -> Result<ExitCode, Error> {
    thread::Builder::new()
        .name("dffy-session".into())
        .spawn(|| {
            if session::ended() {
                leave();
            }
        })?;
    let worktree = crate::base_dir(dir)
        .ok()
        .and_then(|dir| client::worktree_of(&dir))
        .and_then(|w| w.into_os_string().into_string().ok());
    let Some(worktree) = worktree else {
        loop {
            thread::park();
        }
    };
    let mut params = vec![("worktree", text(worktree))];
    params.extend(key.map(|k| ("key", text(k))));
    let params = object(params);
    let mut said = HashSet::new();
    let mut wait = BACKOFF.start;
    loop {
        let since = Instant::now();
        match client::connect_or_start(name) {
            Ok(mut client) => attend(&mut client, &params, &mut said)?,
            Err(e) => say(
                &mut said,
                format!("dffyd can't be reached ({e}); trying again"),
            ),
        }
        if since.elapsed() >= BACKOFF.end {
            wait = BACKOFF.start;
        }
        thread::sleep(wait);
        wait = (wait * 2).min(BACKOFF.end);
    }
}

/// Prints the prompts handed over on `client` until its dffyd is gone.
fn attend(client: &mut Client, params: &Value, said: &mut HashSet<String>) -> io::Result<()> {
    loop {
        match client.call("comments/listen", params.clone()) {
            Ok(result) => {
                let Some(prompt) = result.get("prompt").and_then(Value::as_str) else {
                    return Ok(());
                };
                let mut out = io::stdout().lock();
                writeln!(out, "{}", crate::render::plain(prompt))?;
                out.flush()?;
            }
            Err(fault) if fault.code == "unknown_method" => {
                say(
                    said,
                    "the running dffyd is older than this dffy and can't hand over threads; \
                     replace it with: dffy daemon stop"
                        .to_owned(),
                );
                client.wait_gone();
                return Ok(());
            }
            Err(_) => return Ok(()),
        }
    }
}

/// Says `message` on stderr, once.
fn say(said: &mut HashSet<String>, message: String) {
    if !said.contains(&message) {
        writeln!(
            io::stderr().lock(),
            "dffy: {}",
            crate::render::plain(&message)
        )
        .ok();
        said.insert(message);
    }
}

/// `dffy comments listening`: says that the session listening with `key`
/// just edited a file, or was just prompted. Nobody may be listening, so a
/// failure is nothing to say.
pub(crate) fn listening(key: &str, edited: bool) {
    let Ok(Some(mut client)) = client::connect_within("dffy comments", REPORT) else {
        return;
    };
    let params = object(vec![("key", text(key)), ("edited", Value::Bool(edited))]);
    client.call("comments/listening", params).ok();
}

#[expect(
    clippy::exit,
    reason = "ends the process from outside the thread that waits on dffyd"
)]
fn leave() -> ! {
    std::process::exit(0)
}

/// The end of the session that reads a listener: its parent exits, or
/// nobody reads its stdout any more. A signal ends the listener by itself.
///
/// What stays unseen: the session's end behind a parent that outlives it,
/// such as a shell that didn't `exec` the listener, when stdout is a file or
/// `/dev/null` and so has no reader; and a parent that was gone before the
/// listener looked.
mod session {
    #[cfg(target_os = "macos")]
    use macos as system;
    #[cfg(not(target_os = "macos"))]
    use polled as system;

    /// Waits for the session's end; false when the system can't say.
    pub(super) fn ended() -> bool {
        system::ended()
    }

    /// macOS polls neither processes nor the write end alone, so a kernel
    /// queue does.
    #[cfg(target_os = "macos")]
    mod macos {
        use nix::errno::Errno;
        use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent, Kqueue};

        pub(super) fn ended() -> bool {
            let Ok(queue) = Kqueue::new() else {
                return false;
            };
            let event = |ident, filter, flags, note| KEvent::new(ident, filter, flags, note, 0, 0);
            // Cleared once seen, so it isn't woken by stdout staying
            // writable, only by its becoming so.
            let stdout = event(
                1,
                EventFilter::EVFILT_WRITE,
                EventFlag::EV_ADD | EventFlag::EV_CLEAR,
                FilterFlag::empty(),
            );
            let parent = rustix::process::getppid();
            let exit = parent
                .and_then(|pid| usize::try_from(pid.as_raw_nonzero().get()).ok())
                .map(|pid| {
                    event(
                        pid,
                        EventFilter::EVFILT_PROC,
                        EventFlag::EV_ADD,
                        FilterFlag::NOTE_EXIT,
                    )
                });
            // One at a time: one the system refuses doesn't lose the other.
            for change in std::iter::once(stdout).chain(exit) {
                queue.kevent(&[change], &mut [], None).ok();
            }
            if rustix::process::getppid() != parent {
                return true;
            }
            let mut events = [stdout; 2];
            loop {
                let seen = match queue.kevent(&[], &mut events, None) {
                    Ok(seen) => seen,
                    Err(Errno::EINTR) => continue,
                    Err(_) => return false,
                };
                if events[..seen].iter().any(|e| {
                    e.filter() == Ok(EventFilter::EVFILT_PROC)
                        || e.flags().contains(EventFlag::EV_EOF)
                }) {
                    return true;
                }
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    mod polled {
        use std::os::fd::OwnedFd;

        use rustix::event::{PollFd, PollFlags, poll};
        use rustix::io::Errno;
        use rustix::process::{Pid, getppid};

        pub(super) fn ended() -> bool {
            let parent = getppid();
            let exit = parent.and_then(watch);
            if getppid() != parent {
                return true;
            }
            let stdout = std::io::stdout();
            // With no events asked for, only the reader going is reported.
            let mut fds = vec![PollFd::new(&stdout, PollFlags::empty())];
            fds.extend(exit.iter().map(|fd| PollFd::new(fd, PollFlags::IN)));
            loop {
                match poll(&mut fds, None) {
                    Ok(_) => return true,
                    Err(Errno::INTR) => {}
                    Err(_) => return false,
                }
            }
        }

        /// Readable once `pid` exits; `None` before Linux 5.3.
        #[cfg(target_os = "linux")]
        fn watch(pid: Pid) -> Option<OwnedFd> {
            use rustix::process::{PidfdFlags, pidfd_open};
            pidfd_open(pid, PidfdFlags::empty()).ok()
        }

        #[cfg(not(target_os = "linux"))]
        fn watch(_: Pid) -> Option<OwnedFd> {
            None
        }
    }
}
