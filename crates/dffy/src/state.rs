//! What dffy remembers between runs: the view choices last made in the
//! viewer and whether the first-run tip was shown. It lives in
//! `$XDG_STATE_HOME/dffy/state`, never in the config directory, which may
//! be a read-only dotfiles checkout.
//!
//! The file is `key = value` lines under a `version` line. Several dffy
//! processes may write it: each re-reads it and changes only the keys it
//! changed, and the last writer wins. Keys this version doesn't know are
//! ignored but written back as they were, so an older dffy doesn't erase a
//! newer one's choices. Anything else wrong with it (unreadable, another
//! version, a line that isn't `key = value`) makes it count as empty, and
//! the next write replaces it.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError, TryLockError};

const VERSION: &str = "1";

/// Config keys a choice in the viewer is remembered for.
pub(crate) const REMEMBERED: &[&str] = &[
    "view",
    "wrap",
    "tree",
    "tree-width",
    "all-files",
    "status-bar",
    "indicators",
    "modified-lines",
    "syntax-diff",
    "rendered",
    "highlight",
    "theme",
    "mouse",
    "line-numbers",
    "hunk-headers",
    "copy-decorations",
    "context",
    "file-view",
    "ignore-all-space",
    "ignore-space-change",
    "watch",
];

/// Set once the first-run tip was shown.
pub(crate) const TIP_SHOWN: &str = "tip-shown";

/// Choices made but not written yet, for [`flush_on_panic`]'s hook: the
/// viewer's own list is out of reach of a panic on another thread, and
/// release builds abort on panic, so nothing unwinds back to the viewer.
static STAGED: Mutex<Vec<(&'static str, String)>> = Mutex::new(Vec::new());

/// `$XDG_STATE_HOME/dffy/state`, falling back to `~/.local/state`.
pub(crate) fn path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    let base =
        xdg.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("dffy").join("state"))
}

/// The remembered `(key, value)` pairs this version knows; none when
/// there's no usable file.
pub(crate) fn read() -> Vec<(String, String)> {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|text| known(&text))
        .unwrap_or_default()
}

fn known(text: &str) -> Vec<(String, String)> {
    parse(text)
        .into_iter()
        .filter(|(k, _)| REMEMBERED.contains(&k.as_str()) || k == TIP_SHOWN)
        .collect()
}

/// Every pair in the file, known or not.
fn parse(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut version = None;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Vec::new();
        };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() {
            return Vec::new();
        }
        if key == "version" {
            version = Some(value.to_owned());
        } else {
            pairs.push((key.to_owned(), value.to_owned()));
        }
    }
    if version.as_deref() == Some(VERSION) {
        pairs
    } else {
        Vec::new()
    }
}

/// Sets `changes` in the file, keeping the other keys as the file has
/// them now. Fails silently: remembering is a convenience.
pub(crate) fn write(changes: &[(&str, String)]) {
    let Some(path) = path() else { return };
    write_to(&path, changes).ok();
}

/// Notes `pending`, the choices not written yet, for a panic to write.
pub(crate) fn stage(pending: &[(&'static str, String)]) {
    let mut staged = STAGED.lock().unwrap_or_else(PoisonError::into_inner);
    if staged.as_slice() != pending {
        *staged = pending.to_vec();
    }
}

/// Makes a panic write what [`stage`] last noted, then run the hook that
/// was installed before. The write is skipped, rather than waited for, if
/// the panic came while another thread was staging.
pub(crate) fn flush_on_panic() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let staged = match STAGED.try_lock() {
            Ok(staged) => Some(staged),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        if let Some(mut staged) = staged
            && !staged.is_empty()
        {
            write(&std::mem::take(&mut *staged));
        }
        previous(info);
    }));
}

fn write_to(path: &std::path::Path, changes: &[(&str, String)]) -> std::io::Result<()> {
    // Numbers each write's temporary file: a panic hook can write while the
    // main thread is, and a shared name would let one rename the other's
    // half-written file.
    static WRITES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let mut pairs = std::fs::read_to_string(path)
        .map(|text| parse(&text))
        .unwrap_or_default();
    for (key, value) in changes {
        match pairs.iter_mut().find(|(k, _)| k == key) {
            Some(pair) => pair.1.clone_from(value),
            None => pairs.push(((*key).to_owned(), value.clone())),
        }
    }
    let dir = path.parent().unwrap_or(path);
    std::fs::create_dir_all(dir)?;
    let mut text = format!(
        "# What dffy remembers between runs; set defaults in its config file instead.\nversion = {VERSION}\n"
    );
    for (key, value) in &pairs {
        let _ = writeln!(text, "{key} = {value}");
    }
    // A file of its own, renamed over the old one, so a reader never sees
    // half of it.
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = dir.join(format!(".state.{}.{n}", std::process::id()));
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        std::fs::remove_file(&temp).ok();
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn writes_merge_with_what_another_process_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dffy/state");
        write_to(&path, &[("wrap", "true".into())]).expect("write");
        write_to(&path, &[("view", "unified".into())]).expect("write");
        write_to(&path, &[("wrap", "false".into())]).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            known(&text),
            pairs(&[("wrap", "false"), ("view", "unified")])
        );
    }

    #[test]
    fn a_corrupt_or_foreign_file_counts_as_empty_and_is_replaced() {
        for text in [
            "garbage\n",
            "version = 9\nwrap = true\n",
            "version = 1\nwrap = true\n= true\n",
        ] {
            assert_eq!(parse(text), [], "{text}");
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state");
        std::fs::write(&path, b"\xff\xfe not text").expect("write");
        write_to(&path, &[("wrap", "true".into())]).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(known(&text), pairs(&[("wrap", "true")]));
    }

    #[test]
    fn unknown_keys_are_ignored_on_read_and_kept_on_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state");
        std::fs::write(
            &path,
            "version = 1\nfuture-thing = on\nwrap = true\nfold = 3\n",
        )
        .expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(known(&text), pairs(&[("wrap", "true")]));
        write_to(&path, &[("wrap", "false".into()), ("watch", "true".into())]).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            parse(&text),
            pairs(&[
                ("future-thing", "on"),
                ("wrap", "false"),
                ("fold", "3"),
                ("watch", "true")
            ])
        );
        assert_eq!(known(&text), pairs(&[("wrap", "false"), ("watch", "true")]));
    }
}
