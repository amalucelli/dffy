//! `--stat`, `--name-only` and `--name-status`, laid out like git's so
//! scripts and eyes trained on git read them the same way.

use unicode_width::UnicodeWidthStr;

use super::Canvas;
use crate::theme::Role;

pub(crate) struct StatEntry {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    /// Sizes when the file is binary.
    pub binary: Option<(u64, u64)>,
}

/// Git's diffstat for a `width`-column output (git uses 80 off a terminal).
pub(crate) fn stat(entries: &[StatEntry], width: usize, c: &mut Canvas) {
    let decimal_width = |n: u64| n.max(1).ilog10() as usize + 1;
    let mut max_change = 0;
    let mut max_len = 0;
    let mut number_width = 0;
    let mut bin_width = 0;
    for e in entries {
        max_len = max_len.max(e.path.width());
        if let Some((old, new)) = e.binary {
            number_width = 3;
            bin_width = bin_width.max(14 + decimal_width(old) + decimal_width(new));
        } else {
            max_change = max_change.max(e.added + e.removed);
        }
    }
    number_width = number_width.max(decimal_width(max_change as u64));
    let width = width.max(16 + 6 + number_width);
    let mut graph_width = if max_change + 4 > bin_width {
        max_change
    } else {
        bin_width - 4
    };
    let mut name_width = max_len;
    if name_width + number_width + 6 + graph_width > width {
        let graph_cap = (width * 3 / 8).saturating_sub(number_width + 6);
        if graph_width > graph_cap {
            graph_width = graph_cap.max(6);
        }
        let name_cap = width.saturating_sub(number_width + 6 + graph_width);
        if name_width > name_cap {
            name_width = name_cap;
        } else {
            graph_width = width - number_width - 6 - name_width;
        }
    }

    let (mut insertions, mut deletions) = (0, 0);
    for e in entries {
        let (prefix, name) = fit_name(&e.path, name_width);
        let padding = name_width.saturating_sub(prefix.len() + name.width());
        c.push_fmt(
            Role::Plain,
            format_args!(" {prefix}{name}{:padding$} | ", ""),
        );
        if let Some((old, new)) = e.binary {
            c.push_fmt(Role::Plain, format_args!("{:>number_width$}", "Bin"));
            if old != 0 || new != 0 {
                c.push(Role::Plain, " ");
                c.push(Role::Removed, &old.to_string());
                c.push(Role::Plain, " -> ");
                c.push(Role::Added, &new.to_string());
                c.push(Role::Plain, " bytes");
            }
            c.end_line();
            continue;
        }
        insertions += e.added;
        deletions += e.removed;
        let (mut add, mut del) = (e.added, e.removed);
        if graph_width <= max_change {
            let scale = |n: usize| {
                if n == 0 {
                    0
                } else {
                    1 + n * (graph_width - 1) / max_change
                }
            };
            let mut total = scale(add + del);
            if total < 2 && add > 0 && del > 0 {
                total = 2;
            }
            if add < del {
                add = scale(add);
                del = total - add;
            } else {
                del = scale(del);
                add = total - del;
            }
        }
        let changed = e.added + e.removed;
        c.push_fmt(Role::Plain, format_args!("{changed:>number_width$}"));
        if changed > 0 {
            c.push(Role::Plain, " ");
        }
        c.push(Role::Added, &"+".repeat(add));
        c.push(Role::Removed, &"-".repeat(del));
        c.end_line();
    }
    summary(entries.len(), insertions, deletions, c);
}

/// git's ` N files changed, X insertions(+), Y deletions(-)` line.
fn summary(files: usize, insertions: usize, deletions: usize, c: &mut Canvas) {
    if files == 0 {
        c.line(Role::Plain, " 0 files changed");
        return;
    }
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    c.push_fmt(
        Role::Plain,
        format_args!(" {files} file{} changed", plural(files)),
    );
    if insertions > 0 || deletions == 0 {
        c.push(Role::Plain, ", ");
        c.push_fmt(
            Role::Added,
            format_args!("{insertions} insertion{}(+)", plural(insertions)),
        );
    }
    if deletions > 0 || insertions == 0 {
        c.push(Role::Plain, ", ");
        c.push_fmt(
            Role::Removed,
            format_args!("{deletions} deletion{}(-)", plural(deletions)),
        );
    }
    c.end_line();
}

/// Shortens `name` to `width` columns the way git does: keep the tail,
/// prefix `...`, and start at a directory boundary when there is one.
fn fit_name(name: &str, width: usize) -> (&'static str, &str) {
    if name.width() <= width {
        return ("", name);
    }
    let keep = width.saturating_sub(3);
    let mut start = 0;
    let mut remaining = name.width();
    for (i, ch) in name.char_indices() {
        if remaining <= keep {
            start = i;
            break;
        }
        remaining -= unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        start = i + ch.len_utf8();
    }
    let tail = &name[start..];
    let tail = tail.find('/').map_or(tail, |slash| &tail[slash..]);
    ("...", tail)
}

/// A rename's name in a diffstat, as git's `pprint_rename` writes it: the
/// common leading directories and trailing part outside braces,
/// `dir/{old => new}/file`.
pub(crate) fn rename_name(from: &[u8], to: &[u8]) -> String {
    let (a, b) = (from, to);
    let mut prefix = 0;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if x != y {
            break;
        }
        if *x == b'/' {
            prefix = i + 1;
        }
    }
    // Walk back from the ends (one past each, like git's terminating NUL)
    // while the bytes agree, at most into the prefix's slash.
    let back = usize::from(prefix > 0);
    let (mut i, mut j) = (a.len(), b.len());
    let mut suffix = 0;
    let at = |s: &[u8], k: usize| s.get(k).copied();
    while i + back >= prefix && j + back >= prefix && at(a, i) == at(b, j) {
        if at(a, i) == Some(b'/') {
            suffix = a.len() - i;
        }
        if i == 0 || j == 0 {
            break;
        }
        i -= 1;
        j -= 1;
    }
    let a_mid = a.len().saturating_sub(prefix + suffix);
    let b_mid = b.len().saturating_sub(prefix + suffix);
    let (pre, post) = (&a[..prefix], &a[a.len() - suffix..]);
    let (old, new) = (&a[prefix..prefix + a_mid], &b[prefix..prefix + b_mid]);
    let (old, new) = (super::escape(old), super::escape(new));
    if prefix + suffix > 0 {
        format!(
            "{}{{{old} => {new}}}{}",
            super::escape(pre),
            super::escape(post)
        )
    } else {
        format!("{old} => {new}")
    }
}

/// The `--name-status` letter and its role.
pub(crate) fn status_letter(status: super::FileStatus) -> (&'static str, Role) {
    use super::FileStatus as S;
    match status {
        S::Added => ("A", Role::Added),
        S::Deleted => ("D", Role::Removed),
        S::Modified => ("M", Role::Warn),
        S::Renamed => ("R", Role::Hint),
        S::Copied => ("C", Role::Hint),
        S::TypeChange => ("T", Role::Hint),
        S::Untracked => ("?", Role::Dim),
        S::Combined => ("U", Role::Warn),
    }
}

/// A file's staging as the viewer words it after its status, as in
/// `modified · partly staged`.
pub(crate) fn staging_word(staging: dffy_core::git::Staging) -> &'static str {
    use dffy_core::git::Staging as S;
    match staging {
        S::Staged => "staged",
        S::Unstaged => "unstaged",
        S::Partly => "partly staged",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, added: usize, removed: usize) -> StatEntry {
        StatEntry {
            path: path.to_owned(),
            added,
            removed,
            binary: None,
        }
    }

    #[test]
    fn matches_git_layout_and_scaling() {
        let entries = [
            entry("src/main.rs", 120, 30),
            entry("README.md", 1, 0),
            entry(
                "a/very/long/directory/name/that/does/not/fit/in/the/name/column/file.rs",
                0,
                2,
            ),
            StatEntry {
                path: "logo.png".to_owned(),
                added: 0,
                removed: 0,
                binary: Some((1024, 2048)),
            },
        ];
        let mut c = Canvas::default();
        stat(&entries, 80, &mut c);
        insta::assert_snapshot!(c.markup());
    }

    #[test]
    fn summary_pluralizes_and_omits_zero_sides_like_git() {
        let mut c = Canvas::default();
        summary(1, 1, 0, &mut c);
        summary(2, 0, 3, &mut c);
        summary(0, 0, 0, &mut c);
        summary(3, 0, 0, &mut c);
        insta::assert_snapshot!(c.markup(), @r"
         1 file changed, <added>1 insertion(+)</>
         2 files changed, <removed>3 deletions(-)</>
         0 files changed
         3 files changed, <added>0 insertions(+)</>, <removed>0 deletions(-)</>
        ");
    }
}
