//! Go to definition in a helper process: `dffy __navigate` owns the
//! navigators of one repository for one host (the viewer, or the MCP
//! server), builds their index of definitions on threads of its own, and
//! answers lookups and searches over JSON-RPC 2.0 on its stdin and stdout
//! (docs/navigate.md). The index lives in that process alone: an allocator
//! keeps memory once it's freed, so only the helper's exit gives it back,
//! and the helper exits when idle or when its host goes away.
//!
//! [`wire`] writes and reads the protocol's values for both sides, the
//! server is [`server`], and [`client`] is a host's handle on it.

pub(crate) mod client;
pub(crate) mod server;
pub(crate) mod wire;

#[cfg(test)]
mod tests;

/// The profile's folder, from the test binary `exe` somewhere in it: the
/// nearest with a `dffy`, or with the copies of it made before, as while
/// the `dffy` is relinked.
#[cfg(test)]
fn profile(exe: &std::path::Path) -> Option<&std::path::Path> {
    (exe.ancestors().skip(1)).find(|dir| dir.join("dffy").is_file() || dir.join(COPIES).is_dir())
}

/// The folder of the copies [`built_program`] makes, in the profile's.
#[cfg(test)]
const COPIES: &str = "test-helpers";

/// The dffy binary built with the unit tests, which aren't dffy
/// themselves: what their helpers run as, the profile's `dffy`, and only
/// when it says it is this very build: one left from an older build is
/// refused.
///
/// The helpers run a copy of it, one per build, in the profile's
/// `test-helpers/`: a build relinking the profile's `dffy` while tests run
/// (another `cargo build` on the checkout) would otherwise leave a helper
/// nothing to start while it's rewritten. The copy is taken only of a
/// binary answering as this build, asked again for a while as it may be
/// halfway written, and moved into place whole; once there, the profile's
/// `dffy` isn't read again.
#[cfg(test)]
pub(crate) fn built_program() -> std::path::PathBuf {
    static COPY: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    COPY.get_or_init(|| {
        let exe = std::env::current_exe().unwrap_or_default();
        let profile = profile(&exe)
            .unwrap_or_else(|| panic!("no dffy built above {}: build it", exe.display()))
            .to_owned();
        // What `path` says it is, in at most a few seconds: an exec racing a
        // relink of the file can hang before it runs any of it.
        let build = |path: &std::path::Path| {
            use std::io::Read as _;
            use std::process::{Command, Stdio};
            let Ok(mut child) = Command::new(path)
                .args([COMMAND, "--build"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                return String::new();
            };
            let start = std::time::Instant::now();
            while matches!(child.try_wait(), Ok(None)) {
                if start.elapsed() > Duration::from_secs(5) {
                    child.kill().ok();
                    child.wait().ok();
                    return String::new();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let mut said = String::new();
            (child.stdout.take()).map(|mut out| out.read_to_string(&mut said).ok());
            said.trim().to_owned()
        };
        let dir = profile.join(COPIES);
        let copy = dir.join(format!("dffy-{}", crate::BUILD));
        if build(&copy) == crate::BUILD {
            return copy;
        }
        let found = profile.join("dffy");
        let start = std::time::Instant::now();
        loop {
            let said = build(&found);
            if said == crate::BUILD {
                break;
            }
            // Another build's answer is final; none may be a binary being written.
            assert!(
                said.is_empty() && start.elapsed() < Duration::from_secs(30),
                "{} isn't of this build (it says {said:?}): build it",
                found.display()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        std::fs::create_dir_all(&dir).expect("a folder for the helpers' binary");
        let part = dir.join(format!("dffy-{}.{}", crate::BUILD, std::process::id()));
        std::fs::copy(&found, &part).expect("the helpers' binary copied");
        std::fs::rename(&part, &copy).expect("the copy moved into place");
        // Older builds' copies: fine to drop once nothing started lately runs them.
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let stale = (entry.metadata().and_then(|m| m.modified())).is_ok_and(|t| t < old);
            if stale && entry.path() != copy {
                std::fs::remove_file(entry.path()).ok();
            }
        }
        assert_eq!(build(&copy), crate::BUILD, "the copy answers as this build");
        copy
    })
    .clone()
}

use std::time::Duration;

/// A name whose lookup a debug build's helper exits on, as on a file that
/// overflows its stack, for tests of what its hosts do then.
#[cfg(debug_assertions)]
pub(crate) const DEADLY: &str = "dffy_deadly_name";

/// The first argument that makes `dffy` the helper.
pub(crate) const COMMAND: &str = "__navigate";

/// The protocol version, sent in `initialize` and named in its answer; a
/// host of another version is refused.
pub(crate) const PROTOCOL: usize = 1;

/// How long the helper waits for a request before it exits.
const IDLE: Duration = Duration::from_secs(10 * 60);

/// Navigators the helper keeps at once: a diff's two sides.
const NAVIGATORS: usize = 2;

/// Searches the helper keeps at once.
const SEARCHES: usize = 8;
