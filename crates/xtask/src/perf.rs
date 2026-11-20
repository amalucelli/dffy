//! `cargo xtask perf`: the checkout measured A/B against a commit, the
//! baseline, which defaults to `HEAD~1`.
//!
//! Every run builds its baseline set fresh, from a temporary git worktree
//! of that commit, under `target/perf/`, and removes everything there but
//! the results in `runs/` and the layout bands in `layout.tsv` when it ends,
//! failed, interrupted or not, and again when the next run starts. There's
//! no per-baseline calibration:
//! allocations get no noise band, and timings are held to the layout bands,
//! shared by every baseline, which `--full` measures again when missing or
//! older than [`LAYOUT_MAX_AGE_DAYS`], and for a group they have no band
//! of, such as a new one, alone. Every tier measures every group;
//! `--only` narrows the normal and full tiers.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ab::{self, GROUPS, TIMINGS_ONLY};
use crate::results::short;
use crate::{ROOT, Result};

const USAGE: &str = "usage: cargo xtask perf [--quick | --full] [--rev REV] [--rounds N] [--only GROUP,...] [--allow-missing-baseline]";

/// The layouts `--full` builds to measure layout bands.
const LAYOUTS: usize = 4;

/// How old `target/perf/layout.tsv` may get before `--full` measures the
/// layout bands again.
const LAYOUT_MAX_AGE_DAYS: u64 = 30;

/// What a run leaves in `target/perf`.
const KEPT: [&str; 2] = ["layout.tsv", "runs"];

pub(crate) fn main(args: &[String]) -> Result<bool> {
    let mut tier = "normal";
    let mut rev = None;
    let mut rounds = None;
    let mut only: Option<Vec<String>> = None;
    let mut allow_missing_baseline = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--quick" => tier = "quick",
            "--full" => tier = "full",
            "--rounds" => rounds = Some(ab::rounds_arg(args.next())?),
            "--only" => only = Some(ab::only_arg(args.next())?),
            "--allow-missing-baseline" => allow_missing_baseline = true,
            "--rev" => {
                rev = Some(
                    args.next()
                        .ok_or("--rev takes a commit, branch or tag")?
                        .as_str(),
                );
            }
            _ => return Err(USAGE.to_owned()),
        }
    }
    if tier == "quick" {
        ab::quick_supported()?;
        if only.is_some() {
            return Err("--quick measures every group; drop --only".to_owned());
        }
    }
    let rounds = rounds.unwrap_or(if tier == "quick" { 3 } else { 10 });
    let default = rev.is_none();
    let rev = rev.unwrap_or("HEAD~1");
    let commit = git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{rev}^{{commit}}"),
    ])
    .map_err(|_| format!("{rev}: not a commit in this repository"))?;
    let commit = commit.trim();
    eprintln!(
        "baseline: {rev}, commit {}{}",
        short(commit),
        if default {
            " (the default; --rev <rev> picks another)"
        } else {
            ""
        }
    );

    let mut groups = GROUPS.to_vec();
    if let Some(only) = &only {
        groups.retain(|g| only.iter().any(|o| o == g));
    }
    if tier == "quick" {
        for group in TIMINGS_ONLY.iter().filter(|g| groups.contains(g)) {
            eprintln!("quick: {group} has only timings, skipped");
        }
        groups.retain(|g| !TIMINGS_ONLY.contains(g));
    }
    if groups.is_empty() {
        println!("Nothing to measure: --only left no group for this tier.");
        return Ok(true);
    }

    let perf = perf_dir();
    // What a run killed before its cleanup left, such as one whose signal
    // reached only xtask; a leftover worktree would also stop the checkout.
    // A Ctrl-C at the terminal kills the running child too, and the error
    // brings this run to its own cleanup below.
    clean_up(&perf);
    let result = measure(
        &perf,
        commit,
        (tier, rounds, &groups),
        allow_missing_baseline,
    );
    clean_up(&perf);
    result
}

/// The checkout against a fresh build of `commit`, with the builds under
/// `perf`; `allow_missing_baseline` as [`ab::run`] takes it.
fn measure(
    perf: &Path,
    commit: &str,
    (tier, rounds, groups): (&str, usize, &[&str]),
    allow_missing_baseline: bool,
) -> Result<bool> {
    let worktree = perf.join("worktree");
    git(&[
        "worktree",
        "add",
        "--detach",
        "--quiet",
        &worktree.display().to_string(),
        commit,
    ])?;
    let base = perf.join("baseline");
    eprintln!(
        "baseline {}: building it from a worktree of that commit into {}",
        short(commit),
        base.display()
    );
    ab::snapshot(&base, None, Some(&worktree))?;
    let layout = perf.join("layout.tsv");
    let age = age_in_days(&layout);
    let stale = age.is_none_or(|days| days > LAYOUT_MAX_AGE_DAYS);
    let mut calibrate = Vec::new();
    if tier == "full" {
        let lacking: Vec<&str> = (groups.iter().filter_map(|g| ab::needs(g)))
            .filter(|need| !(need.has)(&base.join("dffy"), perf))
            .map(|need| {
                eprintln!(
                    "warning: the baseline's dffy can't {}, so the {} group gets no layout bands from {}",
                    need.what,
                    need.group,
                    short(commit)
                );
                need.group
            })
            .collect();
        calibrate = calibrated(groups, &ab::read_bands(&layout), stale, &lacking);
    }
    if stale && !calibrate.is_empty() {
        let why = age.map_or_else(
            || "there are none".to_owned(),
            |days| format!("they're {days} days old"),
        );
        eprintln!(
            "calibrating: measuring layout bands on {}, since {why}: {LAYOUTS} builds with shuffled code layouts and {} comparisons with the first, about {} times as long as the comparison",
            short(commit),
            LAYOUTS - 1,
            LAYOUTS - 1
        );
        ab::measure_layouts(&base, LAYOUTS, rounds, &calibrate, Some(&worktree), &layout)?;
    } else if !calibrate.is_empty() {
        eprintln!(
            "calibrating: measuring layout bands of {} on {}, since {} has none for them",
            calibrate.join(", "),
            short(commit),
            layout.display()
        );
        // The other groups' bands keep their age.
        let modified = layout.metadata().and_then(|m| m.modified());
        ab::measure_layouts(&base, LAYOUTS, rounds, &calibrate, Some(&worktree), &layout)?;
        if let Ok(time) = modified
            && let Err(e) = fs::File::options()
                .write(true)
                .open(&layout)
                .and_then(|f| f.set_modified(time))
        {
            eprintln!("warning: {}: {e}", layout.display());
        }
    } else if tier != "quick" {
        let measured = fs::read_to_string(&layout).unwrap_or_default();
        match measured.lines().next().and_then(|l| l.strip_prefix("# ")) {
            Some(header) => eprintln!("layout bands: {} ({header})", layout.display()),
            None => eprintln!("layout bands: none; task perf:full measures them"),
        }
    }
    ab::run(
        &base,
        rounds,
        groups,
        tier,
        &layout,
        (&[], allow_missing_baseline),
    )
}

/// The groups `--full` measures layout bands of: every group when the bands
/// are `stale`, else those `bands` has none of; never those whose needs
/// the baseline's dffy `lacks` ([`ab::Needs`]).
fn calibrated<'a>(
    groups: &[&'a str],
    bands: &HashMap<String, f64>,
    stale: bool,
    lacks: &[&str],
) -> Vec<&'a str> {
    groups
        .iter()
        .copied()
        .filter(|g| !lacks.contains(g))
        // Every metric's name starts with its group's.
        .filter(|g| stale || !bands.keys().any(|m| m.starts_with(g)))
        .collect()
}

/// Days since `path` was last written; none when it doesn't exist.
fn age_in_days(path: &Path) -> Option<u64> {
    let modified = path.metadata().ok()?.modified().ok()?;
    let age = modified.elapsed().unwrap_or_default();
    Some(age.as_secs() / 86_400)
}

/// `git args` in the checkout, its stdout.
fn git(args: &[&str]) -> Result<String> {
    crate::output(Command::new("git").args(args).current_dir(ROOT)).map(|(out, _)| out)
}

fn perf_dir() -> PathBuf {
    fs::canonicalize(ROOT)
        .unwrap_or_else(|_| ROOT.into())
        .join("target/perf")
}

/// Removes everything in `perf` but [`KEPT`], drops git's record of the
/// removed worktree, and says what went and how much space that freed.
fn clean_up(perf: &Path) {
    let paths = removable(perf);
    if paths.is_empty() {
        return;
    }
    let freed = remove(&paths);
    if let Err(e) = git(&["worktree", "prune"]) {
        eprintln!("warning: {e}");
    }
    let names: Vec<String> = paths
        .iter()
        .filter_map(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
        .collect();
    eprintln!(
        "cleanup: removed {} from {}, about {}; results stay in {}",
        names.join(", "),
        perf.display(),
        human(freed),
        perf.join("runs").display()
    );
}

/// The entries of `perf` a run removes, sorted.
fn removable(perf: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(perf)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !KEPT.iter().any(|k| e.file_name() == *k))
        .map(|e| e.path())
        .collect();
    paths.sort();
    paths
}

/// Removes `paths`, files or directories, warning about any it can't, and
/// returns the bytes of the files that went, hard links counted each time.
fn remove(paths: &[PathBuf]) -> u64 {
    let mut freed = 0;
    for path in paths {
        let size = size(path);
        let removed = if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        match removed {
            Ok(()) => freed += size,
            Err(e) => eprintln!("warning: {}: {e}", path.display()),
        }
    }
    freed
}

fn size(path: &Path) -> u64 {
    match path.symlink_metadata() {
        Ok(m) if m.is_dir() => fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| size(&e.path()))
            .sum(),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

fn human(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{}.{} GB", bytes / 1_000_000_000, bytes / 100_000_000 % 10)
    } else {
        format!("{} MB", bytes.div_ceil(1_000_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty directory under the system's temporary directory, removed
    /// on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("xtask-{name}-{}", std::process::id()));
            fs::remove_dir_all(&dir).ok();
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn a_run_leaves_only_its_results_and_the_layout_bands() {
        let perf = Scratch::new("cleanup");
        let p = &perf.0;
        for dir in [
            "build/release/deps",
            "worktree/src",
            "baseline/layouts/0",
            "runs/2026-11-16-1200",
        ] {
            fs::create_dir_all(p.join(dir)).unwrap();
        }
        fs::write(p.join("build/release/deps/dffy"), vec![0; 3000]).unwrap();
        fs::write(p.join("baseline/dffy"), vec![0; 2000]).unwrap();
        fs::write(p.join("baseline/layouts/0/dffy"), vec![0; 1000]).unwrap();
        fs::write(p.join("baseline.partial"), vec![0; 500]).unwrap();
        fs::write(p.join("runs/2026-11-16-1200/results.json"), "{}").unwrap();
        fs::write(p.join("layout.tsv"), "# header\n").unwrap();

        let paths = removable(p);
        assert_eq!(
            paths,
            ["baseline", "baseline.partial", "build", "worktree"].map(|n| p.join(n))
        );
        assert_eq!(remove(&paths), 6500);
        let mut left: Vec<_> = fs::read_dir(p)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left, KEPT.map(std::ffi::OsString::from));
        assert!(p.join("runs/2026-11-16-1200/results.json").is_file());
    }

    #[test]
    fn full_calibrates_the_groups_without_bands_and_only_those_the_baseline_can_run() {
        // A metric of each group, named as the group names it.
        let names = [
            "frame/full_frame/table",
            "engine/line_diff/fair.c",
            "pty/first frame",
            "count_pass",
            "paint/range first_1",
            "print/range",
            "pager/range",
            "comments/settled",
        ];
        let bands = |names: &[&str]| names.iter().map(|&n| (n.to_owned(), 0.02)).collect();
        let all = bands(&names);
        let none: [&str; 0] = [];
        let lacks = ["comments"];
        assert_eq!(calibrated(&GROUPS, &all, false, &[]), none);
        let before_comments = bands(&names[..7]);
        assert_eq!(
            calibrated(&GROUPS, &before_comments, false, &[]),
            ["comments"]
        );
        assert_eq!(calibrated(&GROUPS, &before_comments, false, &lacks), none);
        assert_eq!(calibrated(&GROUPS, &all, true, &[]), GROUPS);
        assert_eq!(calibrated(&GROUPS, &all, true, &lacks), GROUPS[..7]);
    }

    #[test]
    fn layout_bands_age_from_their_last_write() {
        let scratch = Scratch::new("layout");
        let path = scratch.0.join("layout.tsv");
        assert_eq!(age_in_days(&path), None);
        fs::write(&path, "# header\n").unwrap();
        assert_eq!(age_in_days(&path), Some(0));
        let month = std::time::Duration::from_secs(31 * 86_400);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - month)
            .unwrap();
        assert!(age_in_days(&path).is_some_and(|days| days > LAYOUT_MAX_AGE_DAYS));
    }
}
