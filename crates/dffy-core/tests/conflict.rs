//! Conflicted files from real merges: the regions read with the index's
//! sides, the files that can't be resolved by region and why, and the file
//! written for a set of choices.
#![expect(
    clippy::expect_used,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::ops::ControlFlow;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

use dffy_core::conflict::{Choice, Choices};
use dffy_core::diff::{Options, Whitespace};
use dffy_core::document::{Document, File, Source};
use dffy_core::git::{FileDiff, Repo, Revisions, Status};
use dffy_core::highlight::Registry;
use dffy_core::view::{Arrangement, HunkHeader};

/// git in `dir`, with the conflict style set and without the user's own
/// config, whose merge drivers would write other markers.
fn git(dir: &Path, style: &str, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(["-c", &format!("merge.conflictStyle={style}")])
        .args(args)
        .output()
        .expect("git runs")
}

fn ok(dir: &Path, args: &[&str]) {
    let out = git(dir, "merge", args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One file's sides: `None` for a side that deletes it.
type Sides<'a> = (&'a str, &'a [u8], Option<&'a [u8]>, Option<&'a [u8]>);

/// A repository where merging `feature` into `main`, in conflict `style`,
/// conflicts on each of `files`: path, base, ours and theirs.
fn merged(style: &str, files: &[Sides<'_>]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    ok(d, &["init", "-q", "-b", "main"]);
    let put = |path: &str, content: Option<&[u8]>| match content {
        Some(c) => std::fs::write(d.join(path), c).expect("write"),
        None => std::fs::remove_file(d.join(path)).expect("remove"),
    };
    let commit = |message: &str| {
        ok(d, &["add", "-A"]);
        ok(d, &["commit", "-q", "-m", message]);
    };
    for (path, base, ..) in files {
        put(path, Some(base));
    }
    commit("base");
    ok(d, &["checkout", "-q", "-b", "feature"]);
    for (path, _, _, theirs) in files {
        put(path, *theirs);
    }
    commit("theirs");
    ok(d, &["checkout", "-q", "main"]);
    for (path, _, ours, _) in files {
        put(path, *ours);
    }
    commit("ours");
    let out = git(d, style, &["merge", "-q", "feature"]);
    assert!(!out.status.success(), "the merge conflicts");
    dir
}

/// `path`'s diff as plain `dffy` shows it.
fn shown(dir: &Path, path: &str) -> (Repo, FileDiff) {
    let repo = Repo::discover(dir).expect("repo");
    let spec = repo
        .resolve_revisions(&Revisions::default())
        .expect("resolves");
    let mut found = None;
    repo.for_each_change(&spec, &[path.into()], |c| {
        found = Some(c);
        ControlFlow::Break(())
    })
    .expect("lists");
    let change = found.expect("listed");
    assert_eq!(change.status, Status::Unmerged);
    let diff = repo.file_diff(change, &Options::default()).expect("diffs");
    (repo, diff)
}

const BASE: &[u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
const OURS: &[u8] = b"one\nTWO\nthree\nfour\nfive\nsix\nSEVEN\n";
const THEIRS: &[u8] = b"one\n2\nthree\nfour\nfive\nsix\n7\n";

#[test]
fn a_merge_conflict_reads_as_regions_and_writes_the_choices() {
    let dir = merged("diff3", &[("a.txt", BASE, Some(OURS), Some(THEIRS))]);
    let d = dir.path();
    std::fs::set_permissions(d.join("a.txt"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    let (repo, diff) = shown(d, "a.txt");
    let conflicts = repo.conflicts(&diff);
    assert_eq!(conflicts.refused, None);
    assert_eq!(conflicts.regions.len(), 2);
    let base = conflicts.regions[0]
        .base
        .clone()
        .expect("diff3 writes the base");
    assert_eq!(diff.new.line(base.start), b"two\n");

    let mut choices = Choices::new(conflicts.regions);
    choices.picks = vec![Some(Choice::Theirs), Some(Choice::Ours)];
    let left = repo
        .write_resolution("a.txt".into(), &choices)
        .expect("writes");
    assert_eq!(left, 0);
    let written = std::fs::read_to_string(d.join("a.txt")).expect("read");
    assert_eq!(written, "one\n2\nthree\nfour\nfive\nsix\nSEVEN\n");
    let mode = std::fs::metadata(d.join("a.txt"))
        .expect("stat")
        .permissions();
    assert_eq!(mode.mode() & 0o777, 0o755, "the file keeps its mode");
    let stray = std::fs::read_dir(d)
        .expect("ls")
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().contains(".dffy-"));
    assert!(!stray, "no temporary file is left");

    // The file changed since: the choices no longer fit it.
    let again = repo.write_resolution("a.txt".into(), &choices);
    assert!(
        matches!(&again, Err(e) if e.to_string().contains("changed since")),
        "{again:?}"
    );
}

/// A file that became a symbolic link or a directory after the choices
/// were made is refused, and what it points at isn't written through; a
/// stale temporary file, or a link put where it goes, isn't followed.
#[test]
fn a_file_no_longer_regular_is_refused_and_links_are_not_followed() {
    let dir = merged("merge", &[("a.txt", BASE, Some(OURS), Some(THEIRS))]);
    let d = dir.path();
    let (repo, diff) = shown(d, "a.txt");
    let mut choices = Choices::new(repo.conflicts(&diff).regions);
    choices.picks = vec![Some(Choice::Ours), Some(Choice::Ours)];
    let conflicted = std::fs::read(d.join("a.txt")).expect("read");

    let target = d.join("elsewhere.txt");
    std::fs::write(&target, &conflicted).expect("write");
    std::fs::remove_file(d.join("a.txt")).expect("rm");
    std::os::unix::fs::symlink(&target, d.join("a.txt")).expect("symlink");
    let refused = repo.write_resolution("a.txt".into(), &choices);
    assert!(
        matches!(&refused, Err(e) if e.to_string().contains("regular file")),
        "{refused:?}"
    );
    assert_eq!(std::fs::read(&target).expect("read"), conflicted);
    assert!(d.join("a.txt").is_symlink(), "the link stays");

    std::fs::remove_file(d.join("a.txt")).expect("rm");
    std::fs::create_dir(d.join("a.txt")).expect("mkdir");
    let refused = repo.write_resolution("a.txt".into(), &choices);
    assert!(
        matches!(&refused, Err(e) if e.to_string().contains("regular file")),
        "{refused:?}"
    );

    std::fs::remove_dir(d.join("a.txt")).expect("rmdir");
    std::fs::write(d.join("a.txt"), &conflicted).expect("write");
    let decoy = d.join("decoy.txt");
    std::fs::write(&decoy, "untouched\n").expect("write");
    let temp = d.join(format!(".a.txt.dffy-{}", std::process::id()));
    std::os::unix::fs::symlink(&decoy, &temp).expect("symlink");
    repo.write_resolution("a.txt".into(), &choices)
        .expect("writes");
    assert_eq!(
        std::fs::read_to_string(&decoy).expect("read"),
        "untouched\n"
    );
    assert_eq!(
        std::fs::read_to_string(d.join("a.txt")).expect("read"),
        String::from_utf8_lossy(OURS)
    );
}

/// A region left without a choice keeps its markers; the count says so.
#[test]
fn regions_without_a_choice_keep_their_markers() {
    let dir = merged("merge", &[("a.txt", BASE, Some(OURS), Some(THEIRS))]);
    let (repo, diff) = shown(dir.path(), "a.txt");
    let mut choices = Choices::new(repo.conflicts(&diff).regions);
    choices.picks[1] = Some(Choice::Both);
    let left = repo
        .write_resolution("a.txt".into(), &choices)
        .expect("writes");
    assert_eq!(left, 1);
    let written = std::fs::read_to_string(dir.path().join("a.txt")).expect("read");
    assert!(
        written.starts_with("one\n<<<<<<< HEAD\nTWO\n=======\n2\n>>>>>>> feature\n"),
        "{written}"
    );
    assert!(written.ends_with("six\nSEVEN\n7\n"), "{written}");
}

/// A CRLF file keeps its line endings, read from disk as they are.
#[test]
fn a_crlf_file_is_written_with_its_line_endings() {
    let (base, ours, theirs) = (crlf(BASE), crlf(OURS), crlf(THEIRS));
    let dir = merged("merge", &[("w.txt", &base, Some(&ours), Some(&theirs))]);
    let (repo, diff) = shown(dir.path(), "w.txt");
    let mut choices = Choices::new(repo.conflicts(&diff).regions);
    assert_eq!(choices.regions.len(), 2);
    choices.picks = vec![Some(Choice::Ours), Some(Choice::Theirs)];
    repo.write_resolution("w.txt".into(), &choices)
        .expect("writes");
    let written = std::fs::read(dir.path().join("w.txt")).expect("read");
    assert_eq!(
        written,
        b"one\r\nTWO\r\nthree\r\nfour\r\nfive\r\nsix\r\n7\r\n".to_vec()
    );
}

fn crlf(s: &[u8]) -> Vec<u8> {
    s.iter()
        .flat_map(|&b| {
            if b == b'\n' {
                vec![b'\r', b'\n']
            } else {
                vec![b]
            }
        })
        .collect()
}

/// A side whose code has a line of seven `=` reads two ways from the
/// markers; the index's sides settle it.
#[test]
fn the_index_sides_settle_a_side_with_a_marker_like_line() {
    let base = b"Title\nbody\n";
    let ours = b"Title\n=======\nours\n";
    let theirs = b"Title\ntheirs\n";
    let dir = merged("merge", &[("doc.txt", base, Some(ours), Some(theirs))]);
    let (repo, diff) = shown(dir.path(), "doc.txt");
    let regions = repo.conflicts(&diff).regions;
    assert_eq!(regions.len(), 1);
    let ours_lines: Vec<&[u8]> = regions[0].ours.clone().map(|l| diff.new.line(l)).collect();
    assert_eq!(ours_lines, [&b"=======\n"[..], b"ours\n"]);
}

static GRAMMARS: LazyLock<Registry> = LazyLock::new(Registry::builtin);

fn unified(_: &HunkHeader<'_>) -> Arrangement {
    Arrangement::Unified
}

/// Entry `file` as a host's worker prepares a conflicted file: its line
/// diff with its conflicts, or the preview of the choices made on it.
fn prepared(repo: &Repo, doc: &Document<()>, file: usize) -> Box<File> {
    let entry = &doc.files()[file];
    let diff = repo
        .file_diff(entry.change().clone(), &Options::default())
        .expect("diffs");
    let conflicts = repo.conflicts(&diff);
    Box::new(File::prepare(&GRAMMARS, None).conflicted(
        diff,
        conflicts,
        entry.choices().cloned(),
        Whitespace::Exact,
        true,
    ))
}

/// The lines (0-based) the row a view on region `region` puts at the top
/// shows, and the region the core says that row is in.
fn top_of(doc: &mut Document<()>, region: usize) -> ([Option<usize>; 2], Option<usize>) {
    let pos = doc.conflict_pos(0, region, unified).expect("a row");
    let lines = doc.row_lines(pos, unified);
    let file = doc.file(0).expect("loaded");
    (
        lines,
        file.conflicted().expect("conflicted").region_at(lines),
    )
}

/// A conflicted file in a document: choosing makes the file want its diff
/// again, which is then the preview, with every region shown and the
/// counts of the line diff; taking the last choice back shows the line
/// diff. Choices that no longer fit the file are dropped when it's loaded.
#[test]
fn a_document_previews_the_choices_made_on_a_conflicted_file() {
    let dir = merged("merge", &[("a.txt", BASE, Some(OURS), Some(THEIRS))]);
    let (repo, _) = shown(dir.path(), "a.txt");
    let spec = repo
        .resolve_revisions(&Revisions::default())
        .expect("resolves");
    let mut doc: Document<()> = Document::default();
    repo.for_each_change(&spec, &[], |c| {
        doc.push(c, Source::Repo);
        ControlFlow::Continue(())
    })
    .expect("lists");
    let load = |doc: &mut Document<()>| {
        assert!(doc.files()[0].wants_diff());
        let file = prepared(&repo, doc, 0);
        let _ = doc.load(0, Ok(file));
    };
    load(&mut doc);
    let counts = doc.files()[0].counts();
    let regions = doc
        .file(0)
        .and_then(File::conflicted)
        .expect("conflicted")
        .regions()
        .to_vec();
    assert_eq!(regions.len(), 2);
    assert_eq!(
        top_of(&mut doc, 1),
        ([None, Some(regions[1].start)], Some(1))
    );

    // Without a base in the markers, there's none to take.
    let refused = doc.choose(0, 1, Some(Choice::Base));
    assert!(
        refused
            .as_ref()
            .is_err_and(|why| why.starts_with("no base in this conflict")),
        "{refused:?}"
    );
    assert!(!doc.files()[0].wants_diff());
    let made = doc.choose(0, 1, Some(Choice::Theirs)).expect("chosen");
    assert_eq!(
        made.map(|c| c.picks.clone()),
        Some(vec![None, Some(Choice::Theirs)])
    );
    load(&mut doc);
    let file = doc.file(0).expect("loaded");
    assert!(file.is_resolution());
    assert_eq!(file.note(), None);
    assert_eq!(doc.files()[0].counts(), counts, "the line diff's counts");
    // The chosen region's top is its removed `<<<<<<<` line; the other
    // region shows unchanged, from its own.
    assert_eq!(
        top_of(&mut doc, 1),
        ([Some(regions[1].start), None], Some(1))
    );
    let (lines, region) = top_of(&mut doc, 0);
    assert_eq!((lines[0], region), (Some(regions[0].start), Some(0)));

    doc.choose(0, 1, None).expect("taken back");
    assert!(doc.files()[0].choices().is_none());
    load(&mut doc);
    assert!(!doc.file(0).expect("loaded").is_resolution());

    // A file whose conflicts changed drops the choices made on it.
    doc.choose(0, 0, Some(Choice::Ours)).expect("chosen");
    std::fs::write(dir.path().join("a.txt"), "resolved by hand\n").expect("write");
    load(&mut doc);
    assert!(doc.files()[0].choices().is_none());
    assert!(!doc.file(0).expect("loaded").is_resolution());
    assert!(
        doc.choose(0, 0, Some(Choice::Ours)).is_err(),
        "no region left"
    );
}

/// A file one side deleted and the other changed, and a binary file,
/// can't be resolved by region; each says why, and which side deleted.
#[test]
fn deleted_and_binary_conflicts_are_refused_with_why() {
    let bin_base = b"\0base".as_slice();
    let dir = merged(
        "merge",
        &[
            ("gone-ours.txt", BASE, None, Some(THEIRS)),
            ("gone-theirs.txt", BASE, Some(OURS), None),
            ("blob.bin", bin_base, Some(b"\0ours"), Some(b"\0theirs")),
        ],
    );
    let refused = |path: &str| {
        let (repo, diff) = shown(dir.path(), path);
        repo.conflicts(&diff).refused.expect("refused")
    };
    assert!(
        refused("gone-ours.txt").starts_with("the current side (ours) deleted this file"),
        "{}",
        refused("gone-ours.txt")
    );
    assert!(
        refused("gone-theirs.txt").starts_with("the incoming side (theirs) deleted this file"),
        "{}",
        refused("gone-theirs.txt")
    );
    assert!(
        refused("blob.bin").contains("binary"),
        "{}",
        refused("blob.bin")
    );
}
