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

/// The dffy binary built with the unit tests, which aren't dffy
/// themselves: what their helpers run as. It's the first `dffy` in the
/// folders above the test binary, the profile's, and only when it says it
/// is this very build: one left from an older build is refused.
#[cfg(test)]
pub(crate) fn built_program() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let found = (exe.ancestors().skip(1))
        .map(|dir| dir.join("dffy"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("no dffy binary above {}: build it", exe.display()));
    let said = std::process::Command::new(&found)
        .args([COMMAND, "--build"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default();
    assert_eq!(
        said,
        crate::BUILD,
        "{} is another build than these tests: build it again",
        found.display()
    );
    found
}

use std::time::Duration;

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
