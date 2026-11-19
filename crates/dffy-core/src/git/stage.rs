//! Staging, unstaging and discarding a hunk or picked lines of an
//! uncommitted diff, as `git add -p`, `git reset -p` and `git checkout -p`
//! do. The patch is built here, from a line diff of the two sides the
//! operation changes between, read when it runs; `git apply` writes it, as
//! it keeps the index's extensions (the cached trees above all) and the
//! working tree's filters right, which writing through gix wouldn't.
//!
//! The lines come from the diff a frontend shows, which may compare other
//! sides: plain `dffy` shows `HEAD` against the working tree whatever is
//! staged. Each picked line is carried over to the operation's diff by the
//! side they share, matched as unchanged lines.

use std::fmt::Write as _;
use std::io::Write as _;
use std::process::{Command, Stdio};

use gix::bstr::{BStr, ByteSlice};

use super::{Error, FileDiff, Mode, Repo, Result, Side, Status};
use crate::diff::{self, Hunk, LineKind, Options, Text, Whitespace};
use crate::view::Staging;

/// What [`Repo::apply_lines`] does with the lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// Puts their working-tree version in the index (`git add -p`).
    Stage,
    /// Puts their `HEAD` version back in the index (`git reset -p`).
    Unstage,
    /// Puts their index version back in the working tree (`git checkout
    /// -p`). An untracked file is deleted instead.
    Discard,
}

impl Edit {
    /// The past tense, for saying what was done: `staged`.
    #[must_use]
    pub fn done(self) -> &'static str {
        match self {
            Self::Stage => "staged",
            Self::Unstage => "unstaged",
            Self::Discard => "discarded",
        }
    }

    /// The verb, for saying what can't be done: `stage`.
    #[must_use]
    pub fn verb(self) -> &'static str {
        match self {
            Self::Stage => "stage",
            Self::Unstage => "unstage",
            Self::Discard => "discard",
        }
    }

    /// The patch goes the other way: it's taken back out of its target.
    fn reverse(self) -> bool {
        self != Self::Stage
    }
}

/// Lines of a shown [`FileDiff`], by 0-based index into its old and new
/// texts. Only the changed ones count; unchanged lines and repeats may be
/// in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picked {
    /// Lines of the old side.
    pub old: Vec<usize>,
    /// Lines of the new side.
    pub new: Vec<usize>,
}

impl Picked {
    /// Every line of `hunk`.
    #[must_use]
    pub fn hunk(hunk: &Hunk) -> Self {
        let mut picked = Self::default();
        for line in &hunk.lines {
            if line.kind != LineKind::Added {
                picked.old.push(line.old);
            }
            if line.kind != LineKind::Removed {
                picked.new.push(line.new);
            }
        }
        picked
    }

    /// How many of them `diff` shows as changed.
    #[must_use]
    pub fn changed(&self, diff: &FileDiff) -> usize {
        let [removed, added] = self.flags(diff);
        removed.iter().chain(&added).filter(|&&f| f).count()
    }

    /// Per line of each side of `diff`: changed there and picked.
    fn flags(&self, diff: &FileDiff) -> [Vec<bool>; 2] {
        let mut flags = [vec![false; diff.old.len()], vec![false; diff.new.len()]];
        for line in diff.hunks.iter().flat_map(|h| &h.lines) {
            match line.kind {
                LineKind::Removed => flags[0][line.old] = true,
                LineKind::Added => flags[1][line.new] = true,
                LineKind::Context => {}
            }
        }
        let mut picked = [vec![false; diff.old.len()], vec![false; diff.new.len()]];
        for (side, lines) in [&self.old, &self.new].into_iter().enumerate() {
            for &i in lines {
                if let Some(p) = picked[side].get_mut(i) {
                    *p = flags[side][i];
                }
            }
        }
        picked
    }
}

/// What [`Repo::apply_lines`] did, or [`Repo::plan_lines`] would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// Changed lines it took: those that differ between the two sides the
    /// operation changes between, which may be fewer than the shown diff
    /// has picked, as when some are staged already.
    pub lines: usize,
    /// It discarded an untracked file by deleting it.
    pub deleted: bool,
    /// The first and last line (1-based) of the operation's newer side
    /// the lines are on, or a removed line's place there: the working tree
    /// for staging and discarding, the index for unstaging.
    pub at: Option<(usize, usize)>,
}

/// One side of the operation's diff: its content and mode, `None` where the
/// file isn't.
type Version = Option<(Text, Mode)>;

impl Edit {
    /// Why `self` can't apply to `diff`'s file at all, whatever lines are
    /// picked, from what the diff already says: no I/O, for a frontend to
    /// ask before it confirms anything.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`] for renamed, conflicted and binary files, files
    /// whose mode or type changed, symbolic links and submodules, files too
    /// large to diff and files shown as read from disk because their filter
    /// failed; and for a discard of a file shown staged, or in a diff whose
    /// new side isn't the working tree.
    pub fn check(self, diff: &FileDiff) -> Result<()> {
        let refuse = |why: &str| Err(Error::Refused(format!("can't {} {why}", self.verb())));
        let change = &diff.change;
        match change.status {
            Status::Renamed { .. } => return refuse("lines of a renamed file; git add -p can"),
            Status::Unmerged => return refuse("a file with a conflict; resolve it first"),
            Status::TypeChange => return refuse("a file whose type changed"),
            _ => {}
        }
        let modes = [change.old, change.new].map(|s| s.map(|s| s.mode()));
        if modes
            .iter()
            .flatten()
            .any(|m| !matches!(m, Mode::File | Mode::Executable))
        {
            return refuse("a symbolic link or a submodule by line");
        }
        if let [Some(old), Some(new)] = modes
            && old != new
        {
            return refuse("lines of a file whose mode changed; git add -p can");
        }
        if diff.binary {
            return refuse("a binary file by line");
        }
        if diff.too_large {
            return refuse("a file too large to diff");
        }
        if diff.note.is_some() {
            return refuse("a file whose filter failed: it's shown as on disk");
        }
        // Discarding puts the index's lines back in the working tree, so
        // only a diff of the working tree shows what it takes.
        let staged = change.staging == Some(Staging::Staged)
            || change
                .new
                .is_some_and(|s| !matches!(s, Side::Worktree { .. }));
        if self == Self::Discard && staged {
            return refuse("staged changes; unstage them first");
        }
        Ok(())
    }
}

impl Repo {
    /// Stages, unstages or discards the `picked` lines of `shown`, a diff of
    /// uncommitted changes as listed by [`super::DiffSpec::Uncommitted`],
    /// [`super::DiffSpec::IndexToWorktree`] or `HEAD` against the index.
    ///
    /// The index, `HEAD` and the working tree are read now, so lines staged
    /// or edited since `shown` was diffed are matched up by content. Picked
    /// lines follow `git add -p`'s edits: staging, a removal left out stays
    /// and an addition left out isn't made; unstaging and discarding take
    /// the patch back out, so there an addition left out stays and a removal
    /// left out isn't undone.
    ///
    /// Discarding an untracked file deletes it, whatever lines are picked.
    /// Discarding never takes a working-tree line edited since `shown` was
    /// diffed: it's refused then, as the edit would be lost unseen.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`] as [`Edit::check`] says, when none of the lines
    /// has anything to do, when a line and its missing newline at the end
    /// of the file would be split, and when discarding lines changed since
    /// they were shown; [`Error::Apply`] when `git apply` fails, as when
    /// the file changed in between; read failures.
    pub fn apply_lines(&self, shown: &FileDiff, picked: &Picked, edit: Edit) -> Result<Applied> {
        let (applied, patch) = self.patch(shown, picked, edit)?;
        let root = self.workdir().ok_or(Error::NoWorktree)?;
        if let Some(patch) = patch {
            git_apply(root, &patch, edit)?;
        } else {
            let full = self.worktree_path(shown.change.path.as_bstr())?;
            std::fs::remove_file(&full).map_err(|source| Error::Io { path: full, source })?;
        }
        Ok(applied)
    }

    /// What [`Self::apply_lines`] would do, without doing it: reads the
    /// index, `HEAD` and the working tree as it would, so a frontend can
    /// say how many lines a discard takes before it asks.
    ///
    /// # Errors
    ///
    /// As [`Self::apply_lines`], but never [`Error::Apply`].
    pub fn plan_lines(&self, shown: &FileDiff, picked: &Picked, edit: Edit) -> Result<Applied> {
        self.patch(shown, picked, edit).map(|(applied, _)| applied)
    }

    /// The patch for [`Self::apply_lines`], `None` for an untracked file to
    /// delete.
    fn patch(
        &self,
        shown: &FileDiff,
        picked: &Picked,
        edit: Edit,
    ) -> Result<(Applied, Option<Vec<u8>>)> {
        edit.check(shown)?;
        let path = shown.change.path.as_bstr();
        let changed_since = || {
            Error::Refused(format!(
                "can't {}: the file changed since it was shown; look again",
                edit.verb()
            ))
        };
        let shown_worktree = matches!(shown.change.new, Some(Side::Worktree { .. }));
        if edit == Edit::Discard && shown.change.status == Status::Untracked {
            let mode = match shown.change.new {
                Some(Side::Worktree { mode, .. }) => mode,
                _ => Mode::File,
            };
            let now = self.worktree_version(path, mode)?;
            if text(&now).bytes() != shown.new.bytes() {
                return Err(changed_since());
            }
            let applied = Applied {
                lines: picked.changed(shown),
                deleted: true,
                at: (!shown.new.is_empty()).then(|| (1, shown.new.len())),
            };
            return Ok((applied, None));
        }
        let index = self.index_version(path)?;
        let worktree_mode = match shown.change.new {
            Some(Side::Worktree { mode, .. }) => mode,
            _ => index.as_ref().map_or(Mode::File, |(_, mode)| *mode),
        };
        let (old, new) = match edit {
            Edit::Stage | Edit::Discard => (index, self.worktree_version(path, worktree_mode)?),
            Edit::Unstage => (self.head_version(path)?, index),
        };
        if let (Some((_, a)), Some((_, b))) = (&old, &new)
            && a != b
        {
            return Err(Error::Refused(format!(
                "can't {} lines of a file whose mode changed; git add -p can",
                edit.verb()
            )));
        }
        if edit == Edit::Discard && old.is_none() {
            return Err(Error::Refused(
                "can't discard: the file isn't in the index to restore it from".to_owned(),
            ));
        }
        let [shown_old, shown_new] = picked.flags(shown);
        let (old_picks, _) = carry(&shown.old, text(&old), &shown_old);
        let (new_picks, edited) = carry(&shown.new, text(&new), &shown_new);
        // Only the working tree holds what nothing else does.
        if edit == Edit::Discard && shown_worktree && edited {
            return Err(changed_since());
        }
        let Some((written, applied)) =
            build(path, (&old, &new), &[old_picks, new_picks], edit.reverse())?
        else {
            return Err(Error::Refused(format!(
                "nothing to {} in the lines picked",
                edit.verb()
            )));
        };
        Ok((applied, Some(written)))
    }

    /// The stage-0 index entry's blob at `path`, `None` without one.
    fn index_version(&self, path: &BStr) -> Result<Version> {
        let index = self.load_index()?;
        let entries = index.entries();
        let first = entries.partition_point(|e| e.path(&index) < path);
        let mut found = None;
        for e in entries[first..]
            .iter()
            .take_while(|e| e.path(&index) == path)
        {
            if e.stage_raw() != 0 {
                return Err(Error::Refused(
                    "this file has a conflict in the index; resolve it first".to_owned(),
                ));
            }
            found = Some((e.id, Mode::from_index(e.mode).unwrap_or(Mode::File)));
        }
        found.map(|(id, mode)| self.blob(id, mode)).transpose()
    }

    /// `HEAD`'s blob at `path`, `None` without one or before the first
    /// commit.
    fn head_version(&self, path: &BStr) -> Result<Version> {
        let repo = self.git()?;
        let tree = repo.head_tree_id_or_empty()?;
        let tree = repo.find_object(tree)?.peel_to_tree()?;
        let Some(entry) = tree.lookup_entry(path.split_str("/"))? else {
            return Ok(None);
        };
        let Some(mode) = Mode::from_tree(entry.mode()) else {
            return Ok(None);
        };
        self.blob(entry.object_id(), mode).map(Some)
    }

    /// The working-tree file at `path` as git would store it, `None` when
    /// it isn't there.
    fn worktree_version(&self, path: &BStr, mode: Mode) -> Result<Version> {
        match self.load(path, &Side::Worktree { mode, id: None }) {
            Ok(bytes) => Ok(Some((Text::new(bytes), mode))),
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn blob(&self, id: gix::ObjectId, mode: Mode) -> Result<(Text, Mode)> {
        let bytes = self.load(BStr::new(""), &Side::Object { id, mode })?;
        Ok((Text::new(bytes), mode))
    }
}

/// The content of `version`; empty where the file isn't.
fn text(version: &Version) -> &Text {
    static EMPTY: std::sync::LazyLock<Text> = std::sync::LazyLock::new(Text::default);
    version.as_ref().map_or(&EMPTY, |(t, _)| t)
}

/// `picked`, flags over `from`'s lines, carried over to the lines of `to`
/// they're unchanged as; and whether a picked line isn't in `to` as it was.
fn carry(from: &Text, to: &Text, picked: &[bool]) -> (Vec<bool>, bool) {
    if from.bytes() == to.bytes() {
        return (picked.to_vec(), false);
    }
    let changes = diff::changes(from, to, Whitespace::Exact);
    let edited = (0..from.len()).any(|i| picked[i] && changes.removed[i]);
    let mut out = vec![false; to.len()];
    let (mut i, mut j) = (0, 0);
    while i < from.len() || j < to.len() {
        if i < from.len() && j < to.len() && !changes.removed[i] && !changes.added[j] {
            out[j] = picked[i];
            i += 1;
            j += 1;
            continue;
        }
        // A picked line edited between the two sides, as a line staged and
        // then edited again, stands for the lines that replaced it.
        let (start, block, mut taken) = (i, j, false);
        while i < from.len() && changes.removed[i] {
            taken |= picked[i];
            i += 1;
        }
        while j < to.len() && changes.added[j] {
            j += 1;
        }
        out[block..j].fill(taken);
        if (i, j) == (start, block) {
            break;
        }
    }
    (out, edited)
}

/// A line of the patch: its sign and its bytes, terminator included.
type PatchLine<'a> = (u8, &'a [u8]);

/// The patch taking the `picks` (flags over the old and the new lines)
/// from `old` to `new`, and what it takes: how many changed lines, and the
/// first and last of `new`'s lines they're on; `None` when it takes none.
/// With `reverse` it's for `git apply -R`: lines left out of it are those
/// of `new` that stay, rather than those of `old`.
fn build(
    path: &BStr,
    (old, new): (&Version, &Version),
    picks: &[Vec<bool>; 2],
    reverse: bool,
) -> Result<Option<(Vec<u8>, Applied)>> {
    let (a, b) = (text(old), text(new));
    let options = Options {
        context: 3,
        whitespace: Whitespace::Exact,
    };
    let mut body = Vec::new();
    let mut taken = 0;
    // New lines minus old ones in the hunks written so far: where the side
    // that isn't the target's starts.
    let mut shift: isize = 0;
    let (mut old_total, mut new_total) = (0, 0);
    let mut at: Option<(usize, usize)> = None;
    let mut mark = |line: usize| {
        let line = (line + 1).min(b.len().max(1));
        at = Some(at.map_or((line, line), |(lo, hi)| (lo.min(line), hi.max(line))));
    };
    for hunk in diff::hunks_without_emphasis(a, b, &options) {
        let mut lines: Vec<PatchLine<'_>> = Vec::new();
        let mut took = 0;
        for line in &hunk.lines {
            match line.kind {
                LineKind::Context => lines.push((b' ', a.line(line.old))),
                LineKind::Removed if picks[0][line.old] => {
                    lines.push((b'-', a.line(line.old)));
                    mark(line.new);
                    took += 1;
                }
                LineKind::Removed if !reverse => lines.push((b' ', a.line(line.old))),
                LineKind::Added if picks[1][line.new] => {
                    lines.push((b'+', b.line(line.new)));
                    mark(line.new);
                    took += 1;
                }
                LineKind::Added if reverse => lines.push((b' ', b.line(line.new))),
                LineKind::Removed | LineKind::Added => {}
            }
        }
        if took == 0 {
            continue;
        }
        check_newlines(&lines)?;
        let count = |signs: &[u8]| lines.iter().filter(|(s, _)| signs.contains(s)).count();
        let (old_len, new_len) = (count(b" -"), count(b" +"));
        let (old_start, new_start) = if reverse {
            (hunk.new_start.saturating_add_signed(-shift), hunk.new_start)
        } else {
            (hunk.old_start, hunk.old_start.saturating_add_signed(shift))
        };
        shift += isize::try_from(new_len).unwrap_or(0) - isize::try_from(old_len).unwrap_or(0);
        let header = |start: usize, len: usize| if len == 0 { start } else { start + 1 };
        body.extend_from_slice(
            format!(
                "@@ -{},{old_len} +{},{new_len} @@\n",
                header(old_start, old_len),
                header(new_start, new_len)
            )
            .as_bytes(),
        );
        for (sign, text) in lines {
            body.push(sign);
            body.extend_from_slice(text);
            if !text.ends_with(b"\n") {
                body.extend_from_slice(b"\n\\ No newline at end of file\n");
            }
        }
        old_total += old_len;
        new_total += new_len;
        taken += took;
    }
    if taken == 0 {
        return Ok(None);
    }
    let mut written = file_header(path, (old, new), (old_total, new_total));
    written.extend_from_slice(&body);
    let applied = Applied {
        lines: taken,
        deleted: false,
        at,
    };
    Ok(Some((written, applied)))
}

/// The patch's file header for `path`, from `old` to `new`, whose hunks
/// keep `totals` lines of each.
fn file_header(path: &BStr, (old, new): (&Version, &Version), totals: (usize, usize)) -> Vec<u8> {
    let (old_total, new_total) = totals;
    // A side the file isn't on makes a creation or a deletion only when no
    // line left out of the patch stands on it as context: staging some
    // lines of a deletion leaves a shorter file in the index.
    let created = old.is_none() && old_total == 0;
    let deleted = new.is_none() && new_total == 0;
    let name = |prefix: &str, absent: bool| {
        if absent {
            return "/dev/null".to_owned();
        }
        let mut full = prefix.as_bytes().to_vec();
        full.extend_from_slice(path);
        quote(full.as_bstr())
    };
    let octal = |v: &Version| match v {
        Some((_, Mode::Executable)) => "100755",
        _ => "100644",
    };
    let mut out = format!("diff --git {} {}\n", name("a/", false), name("b/", false));
    if created {
        let _ = writeln!(out, "new file mode {}", octal(new));
    } else if deleted {
        let _ = writeln!(out, "deleted file mode {}", octal(old));
    }
    let _ = writeln!(
        out,
        "--- {}\n+++ {}",
        name("a/", created),
        name("b/", deleted)
    );
    out.into_bytes()
}

/// Refuses a hunk where a line without its newline, which only the last
/// line of a file can be, has another line after it on the same side: a
/// line's missing newline was picked apart from the line itself.
fn check_newlines(lines: &[PatchLine<'_>]) -> Result<()> {
    for side in *b"-+" {
        let mut on_side = lines.iter().filter(|(s, _)| *s == b' ' || *s == side);
        while let Some((_, text)) = on_side.next() {
            if !text.ends_with(b"\n") && on_side.clone().next().is_some() {
                return Err(Error::Refused(
                    "the last line's missing newline at the end of the file changes too; \
                     pick its old and new versions together"
                        .to_owned(),
                ));
            }
        }
    }
    Ok(())
}

/// A name as git writes it in a patch: in double quotes with C escapes
/// when it has a byte git would quote, or a space, which `git apply`
/// otherwise reads as the end of a name.
fn quote(path: &BStr) -> String {
    let plain = path
        .iter()
        .all(|&b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
    if plain {
        return path.to_str_lossy().into_owned();
    }
    let mut out = String::from("\"");
    for &b in path.iter() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b' '..=b'~' => out.push(char::from(b)),
            _ => {
                let _ = write!(out, "\\{b:03o}");
            }
        }
    }
    out.push('"');
    out
}

/// Runs `git apply` on `patch` in the working tree's root: on the index
/// for staging and unstaging, in reverse for unstaging and discarding.
fn git_apply(root: &std::path::Path, patch: &[u8], edit: Edit) -> Result<()> {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args(["apply", "--whitespace=nowarn"]);
    if edit != Edit::Discard {
        command.arg("--cached");
    }
    if edit.reverse() {
        command.arg("-R");
    }
    let failed = |e: std::io::Error| Error::Apply(format!("couldn't run git: {e}"));
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(failed)?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(patch).map_err(failed)?;
    }
    let out = child.wait_with_output().map_err(failed)?;
    if out.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&out.stderr);
    let first = said
        .lines()
        .find(|l| l.starts_with("error:"))
        .or_else(|| said.lines().next())
        .unwrap_or("it failed");
    Err(Error::Apply(first.trim_start_matches("error: ").to_owned()))
}
