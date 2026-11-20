//! The performance gate's measurement, which `cargo xtask perf` drives,
//! and its parts as the advanced commands `bench-baseline`, `bench-ab`,
//! `bench-noise` and `bench-layouts`.
//!
//! A set is a directory of release executables: `dffy`, the examples
//! `count_pass`, `first_frame` and `first_paint`, and the divan benches
//! `frame` and `engine`, with the commit in `REV`. Two sets, A and B, are
//! measured round by round, alternating which goes first. A metric's value
//! per side is the median of its per-round values.
//!
//! B fails a timing or peak-RSS metric when it was slower than A in enough
//! rounds that a one-sided sign test gives p < 0.05 (9 of 10, or 5 of 5) and
//! its median is above A's by more than 1% (criterion's default noise
//! threshold) or the metric's layout band, whichever is larger: the largest
//! relative difference between sets of one tree built with different code
//! layouts, from `target/perf/layout.tsv` for `perf` and from A's
//! `layout.tsv` for `bench-ab`. It also fails when its instruction count
//! fails. On Linux, the user-space instructions of `count_pass`,
//! `first_paint` and one more run of each hyperfine command are counted, and
//! after the rounds the instructions per iteration of each divan benchmark
//! whose timing failed or that `--instructions` names. A count fails when
//! B's median is more than 0.5% above A's. Divan's allocation counts and
//! bytes fail on any increase, or, when A has a `noise.tsv` from
//! `bench-noise`, on one past its band there: the largest difference between
//! the two sides of one round when A was compared with itself. `count_pass`
//! finding a file whose count differs from its diff fails.
//!
//! The quick tier keeps only the deterministic metrics: instruction and
//! allocation counts, three rounds by default, the divan benchmarks each
//! counted alone, and a subset of the kernel range in place of the whole one
//! for print and pager.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::comments;
use crate::results::{Entry, Header, Results};
use crate::{
    ROOT, Result, Sandbox, TIME, Timing, USAGE, bench_repo, describe, expand_tilde, instructions,
    median, now, peak_rss, quote, time_flag, which,
};

pub(crate) const GROUPS: [&str; 8] = [
    "frame", "engine", "pty", "count", "paint", "print", "pager", "comments",
];

/// The groups with only timings, which the quick tier leaves out.
pub(crate) const TIMINGS_ONLY: [&str; 2] = ["pty", "comments"];

/// A set's executables by cargo target kind and name. cargo also builds
/// test harnesses for the `dffy` lib and bin when it builds the benches, so
/// a match takes kind and name, and bin and example come only from the
/// build, bench only from the bench build.
pub(crate) const EXECUTABLES: [(&str, &str); 6] = [
    ("bin", "dffy"),
    ("example", "count_pass"),
    ("example", "first_frame"),
    ("example", "first_paint"),
    ("bench", "frame"),
    ("bench", "engine"),
];

const SIDES: [&str; 2] = ["A", "B"];

/// The iterations, one per sample, of the two runs of a divan benchmark
/// whose instructions are counted.
const DIVAN_ITERATIONS: [u32; 2] = [50, 250];

/// The iterations per sample of the first of `--quick`'s two runs of a
/// divan benchmark, in [`quick_runs`].
const FIRST_ITERATIONS: u32 = 2;

/// `count_pass`'s files whose count differs from their diff: any in B fails,
/// whatever the noise.
const COUNT_MISMATCH: &str = "count_pass differing files";

/// `--quick`'s stand-in for the whole range in print and pager: 421 of its
/// 12,914 changes, with C sources and headers, Makefiles and Kconfig, Rust,
/// docs, YAML, JSON, 16 renames and the largest file, `rtw8852c_table.c`.
const SUBSET: [&str; 10] = [
    "kernel/sched",
    "mm",
    "include/net",
    "rust",
    "Documentation/rust",
    "Documentation/filesystems",
    "Documentation/devicetree/bindings/pinctrl",
    "tools/perf/pmu-events/arch/arm64",
    "arch/powerpc/kernel",
    "drivers/net/wireless/realtek/rtw89",
];

/// `--quick`'s print runs on [`SUBSET`], and the arguments each adds. Named
/// apart from the full range's runs, so no band measured on the whole range
/// in `noise.tsv` or `layout.tsv` applies to them.
const PRINT_SUBSET: [(&str, &[&str]); 2] = [
    ("print/subset", &[]),
    ("print/subset no-highlight", &["--no-highlight"]),
];

/// `--quick`'s pager run, on [`SUBSET`]'s patch.
const PAGER_SUBSET: &str = "pager/subset";

pub(crate) fn main(cmd: &str, args: &[String]) -> Result<bool> {
    let mut dir = None;
    let mut rounds = None;
    let mut layouts = 4;
    let mut groups = GROUPS.to_vec();
    let mut counted = Vec::new();
    let mut quick = false;
    let mut allow_missing_baseline = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--instructions" if cmd == "bench-ab" => {
                counted.extend(args.next().ok_or(USAGE)?.split(',').map(str::to_owned));
            }
            "--quick" if cmd == "bench-ab" => quick = true,
            "--allow-missing-baseline" if cmd == "bench-ab" => allow_missing_baseline = true,
            "--layouts" if cmd == "bench-layouts" => {
                layouts = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .filter(|&n| n >= 2)
                    .ok_or("--layouts takes a number from 2")?;
            }
            "--rounds" if cmd != "bench-baseline" => rounds = Some(rounds_arg(args.next())?),
            "--only" if cmd != "bench-baseline" => {
                let only = only_arg(args.next())?;
                groups.retain(|g| only.iter().any(|o| o == g));
            }
            _ if dir.is_none() && !arg.starts_with('-') => dir = Some(expand_tilde(arg)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let dir = dir.ok_or(USAGE)?;
    let rounds = rounds.unwrap_or(if quick { 3 } else { 10 });
    let tier = match cmd {
        "bench-baseline" => {
            snapshot(&dir, None, None)?;
            return Ok(true);
        }
        "bench-layouts" => {
            let base = fs::canonicalize(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            measure_layouts(
                &base,
                layouts,
                rounds,
                &groups,
                None,
                &base.join("layout.tsv"),
            )?;
            return Ok(true);
        }
        "bench-ab" if quick => "quick",
        "bench-ab" => "normal",
        "bench-noise" => "noise",
        _ => return Err(USAGE.to_owned()),
    };
    if quick {
        quick_supported()?;
        if !counted.is_empty() {
            return Err("--quick counts every divan benchmark; drop --instructions".to_owned());
        }
        groups.retain(|g| !TIMINGS_ONLY.contains(g));
    }
    let base = baseline(&dir)?;
    run(
        &base,
        rounds,
        &groups,
        tier,
        &base.join("layout.tsv"),
        (&counted, allow_missing_baseline),
    )
}

/// `--rounds`' value.
pub(crate) fn rounds_arg(arg: Option<&String>) -> Result<usize> {
    arg.and_then(|n| n.parse().ok())
        // The sign test's exact counts fit in a u128 to past 100.
        .filter(|n| (1..=100).contains(n))
        .ok_or_else(|| "--rounds takes a number from 1 to 100".to_owned())
}

/// `--only`'s groups.
pub(crate) fn only_arg(arg: Option<&String>) -> Result<Vec<String>> {
    let only: Vec<String> = arg
        .ok_or("--only takes groups")?
        .split(',')
        .map(str::to_owned)
        .collect();
    if let Some(bad) = only.iter().find(|g| !GROUPS.contains(&g.as_str())) {
        return Err(format!(
            "unknown group {bad}; groups are {}",
            GROUPS.join(", ")
        ));
    }
    Ok(only)
}

/// Whether this machine can run the quick tier, which counts instructions.
pub(crate) fn quick_supported() -> Result<()> {
    if cfg!(target_os = "linux") {
        return Ok(());
    }
    Err(format!(
        "the quick tier counts instructions with Linux perf events, which {} doesn't have; use task perf here, or task perf:quick on Linux",
        std::env::consts::OS
    ))
}

/// A feature of dffy a group's scenario needs, which a baseline built from
/// before the feature lacks. Such a group runs only on the sides that have
/// it ([`sides`]), and `--full` measures its layout bands only on a
/// baseline that has it. A group that needs one adds a row to [`NEEDS`].
pub(crate) struct Needs {
    pub group: &'static str,
    /// What it needs, as the messages say it: `open a comments file`.
    pub what: &'static str,
    /// Whether the `dffy` given has it, checked with files under the
    /// directory given, which may not hold them yet.
    pub has: fn(dffy: &Path, dir: &Path) -> bool,
}

pub(crate) const NEEDS: [Needs; 1] = [Needs {
    group: "comments",
    what: "open a comments file",
    has: comments::opens_in,
}];

/// What `group` needs, when it needs something.
pub(crate) fn needs(group: &str) -> Option<&'static Needs> {
    NEEDS.iter().find(|n| n.group == group)
}

/// The sides `need`'s group runs on, from whether each side's `dffy` `has`
/// it. A group measured on B alone compares nothing, so a baseline
/// without it fails the run unless `allow_missing_baseline`; a checkout
/// without it always does.
fn sides(need: &Needs, has: [bool; 2], allow_missing_baseline: bool) -> Result<[bool; 2]> {
    let (group, what) = (need.group, need.what);
    match has {
        [_, false] => Err(format!(
            "B's dffy can't {what}, which the {group} group needs"
        )),
        [false, true] if !allow_missing_baseline => Err(format!(
            "A's dffy can't {what}, so the {group} group would compare nothing; \
             pass --allow-missing-baseline to run it on B alone, or leave it out with --only"
        )),
        [false, true] => {
            eprintln!("warning: A's dffy can't {what}; the {group} group runs only on B");
            Ok(has)
        }
        [true, true] => Ok(has),
    }
}

/// The set in `dir`, canonical, or how to build one.
fn baseline(dir: &Path) -> Result<PathBuf> {
    let missing = |what: String| {
        format!(
            "{}: {what}; build a set there with cargo xtask bench-baseline {}, or use cargo xtask perf, which builds its baselines",
            dir.display(),
            dir.display()
        )
    };
    let base = fs::canonicalize(dir).map_err(|e| missing(e.to_string()))?;
    match EXECUTABLES
        .iter()
        .find(|(_, name)| !base.join(name).is_file())
    {
        Some((_, name)) => Err(missing(format!("no {name}"))),
        None => Ok(base),
    }
}

/// The checkout measured against the set in `base`, the A side, in `tier`:
/// `normal` and `full` measure everything, `quick` only instruction and
/// allocation counts, and `noise` measures `base` against itself and writes
/// its bands to `noise.tsv`. Timings are held to the layout bands in
/// `layout`. `counted` names divan benchmarks to count the instructions of
/// whatever their timing's result; `allow_missing_baseline` runs the
/// comments group on B alone when A can't open its file, which otherwise
/// fails the run.
pub(crate) fn run(
    base: &Path,
    rounds: usize,
    groups: &[&str],
    tier: &str,
    layout: &Path,
    (counted, allow_missing_baseline): (&[String], bool),
) -> Result<bool> {
    let (quick, noise) = (tier == "quick", tier == "noise");
    let base = base.to_path_buf();
    // Before the build, so the commit and uncommitted changes are what's built.
    let built = fs::read_to_string(base.join("REV")).unwrap_or_default();
    let header = Header::gather("perf", tier, built.trim(), groups);
    let mut runner = Runner::new(Sandbox::new(bench_repo()?, false)?, groups, quick)?;
    if quick && !runner.counter {
        return Err(
            "the quick tier needs a hardware instruction counter, and perf_event_open offers none here"
                .to_owned(),
        );
    }
    let candidate = if noise {
        base.clone()
    } else {
        let set = runner.sandbox.path("B");
        snapshot(&set, None, None)?;
        baseline(&set)?
    };
    if quick {
        runner.plan_divan([&base, &candidate], groups)?;
    }

    let mut metrics = runner.rounds(rounds, [&base, &candidate], groups, allow_missing_baseline)?;
    let noise_file = base.join("noise.tsv");
    let layout = read_bands(layout);
    if noise {
        write_noise(&noise_file, &metrics, rounds)?;
    } else {
        let divan: Vec<String> = metrics
            .iter()
            .filter(|m| {
                m.unit == Unit::Time && ["frame/", "engine/"].iter().any(|s| m.name.starts_with(s))
            })
            .filter(|m| counted.contains(&m.name) || timing_fails(m, &layout))
            .map(|m| m.name.clone())
            .collect();
        for name in counted.iter().filter(|n| !divan.contains(n)) {
            eprintln!("warning: --instructions {name}: no divan benchmark of that name measured");
        }
        for name in &divan {
            metrics.push(runner.divan_instructions(name, [&base, &candidate])?);
        }
    }

    let mut summary = format!("# dffy A/B benchmark\n\n{}", header.markdown());
    let b = if noise {
        "the same set"
    } else {
        "the checkout"
    };
    let _ = writeln!(summary, "- Rounds: {rounds}; A: {}; B: {b}", base.display());
    if quick {
        summary += "- Quick: instruction and allocation counts only; print and pager on a subset of the range\n";
    }
    summary.push('\n');
    let (table, pass, verdicts) = table(&metrics, &read_bands(&noise_file), &layout, noise);
    summary += &table;
    if let Some((spread, name)) = largest_spread(&metrics).filter(|_| quick) {
        let _ = write!(
            summary,
            "\nLargest spread of an instruction count over one side's rounds: {:.3}% ({name}).\n",
            spread * 100.0
        );
    }
    if noise {
        let _ = write!(
            summary,
            "\nWrote the bands of {} metrics to {}.\n",
            metrics.len(),
            noise_file.display()
        );
    }
    print!("{summary}");
    let results = Results {
        header,
        metrics: metrics
            .iter()
            .zip(verdicts)
            .map(|(m, result)| Entry {
                name: m.name.clone(),
                unit: m.unit.name().to_owned(),
                series: SIDES
                    .iter()
                    .zip(&m.values)
                    .map(|(side, values)| ((*side).to_owned(), values.clone()))
                    .collect(),
                result,
            })
            .collect(),
        sections: Vec::new(),
    };
    results.write(&runner.raw, &summary)?;
    Ok(pass)
}

/// Builds the current tree and copies its executables into `dest`, with
/// the commit in `dest/REV`. With `layout`, see [`cargo`]. With `worktree`,
/// builds that tree, a worktree of another commit, instead of the checkout.
pub(crate) fn snapshot(dest: &Path, layout: Option<usize>, worktree: Option<&Path>) -> Result<()> {
    fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let tree = worktree.unwrap_or_else(|| Path::new(ROOT));
    let mut found = artifacts(
        cargo(layout, worktree.is_some()),
        tree,
        &["build", "--release", "--workspace", "--examples", "--bins"],
        &["bin", "example"],
    )?;
    found.extend(artifacts(
        cargo(layout, worktree.is_some()),
        tree,
        &["bench", "--workspace", "--no-run"],
        &["bench"],
    )?);
    if found.len() != EXECUTABLES.len() {
        let names: Vec<&str> = found.iter().map(|(name, _)| *name).collect();
        return Err(format!(
            "cargo built {} of the set's executables",
            names.join(", ")
        ));
    }
    for (name, exe) in &found {
        let to = dest.join(name);
        // Copying over an executable that has run can leave macOS holding
        // its old code signature, and the copy is killed on launch.
        if to.exists() {
            fs::remove_file(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        }
        fs::copy(exe, &to).map_err(|e| format!("{} to {}: {e}", exe.display(), to.display()))?;
    }
    // A tree copied without its .git still builds and compares.
    let rev = crate::output(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(tree),
    )
    .map_or_else(|_| "unknown\n".to_owned(), |(rev, _)| rev);
    fs::write(dest.join("REV"), &rev).map_err(|e| format!("{}: {e}", dest.display()))?;
    eprintln!("{}: the set at {}", dest.display(), rev.trim());
    Ok(())
}

/// cargo, building with `layout`, a seed from 1, into its own target
/// directory with the code laid out by the seed: on `x86_64` Linux, where
/// rustc links with lld, lld shuffles the input sections, one per function;
/// elsewhere the seed goes into every symbol's hash. A `worktree` build,
/// with a layout or not, goes to `target/perf/build`, which `perf` removes
/// after every run, so the checkout's `target/release` keeps the checkout's
/// executables.
fn cargo(layout: Option<usize>, worktree: bool) -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    if worktree {
        cmd.env("CARGO_TARGET_DIR", format!("{ROOT}/target/perf/build"));
    }
    let Some(seed) = layout else {
        return cmd;
    };
    // lld takes seed 0 as a random seed.
    let flag = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        format!("-Clink-arg=-Wl,--shuffle-sections=*={seed}")
    } else {
        format!("-Cmetadata=layout{seed}")
    };
    if !worktree {
        cmd.env("CARGO_TARGET_DIR", format!("{ROOT}/target/layouts"));
    }
    // Cargo takes rustflags from the first of these that's set, else from
    // the config files, where the flag joins `build.rustflags` and loses to
    // a `target.<triple>.rustflags`.
    let var = [("CARGO_ENCODED_RUSTFLAGS", '\x1f'), ("RUSTFLAGS", ' ')]
        .into_iter()
        .find_map(|(var, sep)| Some((var, sep, std::env::var(var).ok()?)));
    match var {
        Some((var, _, flags)) if flags.is_empty() => cmd.env(var, flag),
        Some((var, sep, flags)) => cmd.env(var, format!("{flags}{sep}{flag}")),
        None => cmd.arg(format!("--config=build.rustflags=[{flag:?}]")),
    };
    cmd
}

/// The set's executables of the given kinds that `cmd args` built. The
/// paths come from cargo's messages: target/release/deps keeps earlier
/// builds' executables.
fn artifacts(
    mut cmd: Command,
    tree: &Path,
    args: &[&str],
    kinds: &[&str],
) -> Result<Vec<(&'static str, PathBuf)>> {
    cmd.args(args)
        .arg("--message-format=json-render-diagnostics")
        .current_dir(tree)
        .stderr(Stdio::inherit());
    let (stdout, _) = crate::output(&mut cmd)?;
    let mut found: Vec<(&str, PathBuf)> = Vec::new();
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let target = &msg["target"];
        let (Some(kind), Some(name), Some(exe)) = (
            target["kind"][0].as_str(),
            target["name"].as_str(),
            msg["executable"].as_str(),
        ) else {
            continue;
        };
        if let Some(&(_, name)) = EXECUTABLES
            .iter()
            .find(|&&(k, n)| k == kind && n == name && kinds.contains(&k))
            && !found.iter().any(|(n, _)| *n == name)
        {
            found.push((name, PathBuf::from(exe)));
        }
    }
    Ok(found)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Time,
    Bytes,
    Count,
    Instructions,
}

impl Unit {
    fn symbol(self) -> &'static str {
        match self {
            Self::Time => "ns",
            Self::Bytes => "B",
            Self::Count | Self::Instructions => "n",
        }
    }

    /// The unit as `results.json` names it.
    fn name(self) -> &'static str {
        match self {
            Self::Time => "ns",
            Self::Bytes => "B",
            Self::Count => "count",
            Self::Instructions => "instructions",
        }
    }
}

/// A metric's value from one run; none for an instruction count where
/// there's no counter, so the table shows the metric without values.
type Row = (String, Unit, Option<f64>);

struct Metric {
    name: String,
    unit: Unit,
    /// Per side, one value per round.
    values: [Vec<f64>; 2],
}

struct Runner {
    sandbox: Sandbox,
    /// This run's directory under `target/perf/runs`.
    raw: PathBuf,
    /// The range's patch for the pager group, written once per run.
    patch: PathBuf,
    hyperfine: PathBuf,
    time: bool,
    /// Whether the instruction counter opens.
    counter: bool,
    /// `bench-ab --quick`: only instruction and allocation counts.
    quick: bool,
    /// With `quick`, the divan benchmarks to count, from [`Self::plan_divan`].
    divan: Vec<DivanBench>,
}

/// A divan benchmark that `--quick` counts, every round, by running it alone
/// as [`quick_runs`] plans: the difference in instructions over `added` is
/// its instructions per iteration.
struct DivanBench {
    metric: String,
    added: u32,
    /// Whether each side has the benchmark.
    sides: [bool; 2],
}

impl Runner {
    fn new(sandbox: Sandbox, groups: &[&str], quick: bool) -> Result<Self> {
        let raw = crate::results::run_dir()?;
        let needs = |g: &str| groups.contains(&g);
        let hyperfine = which("hyperfine").unwrap_or_default();
        if !quick && (needs("print") || needs("pager")) && hyperfine.as_os_str().is_empty() {
            return Err("hyperfine not found, needed for print and pager".to_owned());
        }
        let time = Path::new(TIME).exists();
        if !time {
            eprintln!("warning: {TIME} not found, peak RSS is skipped");
        }
        let counter = instructions(|| Ok(()))?.1.is_some();
        if !counter {
            eprintln!(
                "warning: no hardware instruction counter (Linux only), instructions are skipped"
            );
        }
        let patch = sandbox.path("range.patch");
        if needs("pager") {
            let out = File::create(&patch).map_err(|e| format!("{}: {e}", patch.display()))?;
            let mut cmd = sandbox.command("git");
            cmd.args(["diff", "--no-ext-diff", "--color=always", "v6.0", "v6.1"]);
            if quick {
                cmd.arg("--").args(SUBSET);
            }
            crate::output(cmd.stdout(out))?;
        }
        Ok(Self {
            sandbox,
            raw,
            patch,
            hyperfine,
            time,
            counter,
            quick,
            divan: Vec::new(),
        })
    }

    /// Lists the divan benchmarks of the suites in `groups` in both sets,
    /// with the [`added_iterations`] of each from its time in A.
    fn plan_divan(&mut self, sets: [&Path; 2], groups: &[&str]) -> Result<()> {
        for suite in ["frame", "engine"]
            .into_iter()
            .filter(|s| groups.contains(s))
        {
            // Per benchmark, its time per iteration in each set.
            let mut times: Vec<(String, [Option<f64>; 2])> = Vec::new();
            for side in 0..2 {
                eprintln!("listing {suite}, {}", SIDES[side]);
                let mut cmd = self.command(sets[side].join(suite))?;
                // Three samples, so the median isn't the cold first one.
                cmd.args(["--bench", "--threads", "1", "--sample-size", "1"])
                    .args(["--sample-count", "3", "--color", "never"])
                    .args(["--bytes-format", "decimal"]);
                let out = self
                    .run(&mut cmd, &format!("list-{}", SIDES[side]), suite)?
                    .0;
                for (name, unit, ns) in parse_divan(suite, &out)? {
                    if unit != Unit::Time {
                        continue;
                    }
                    let i = times
                        .iter()
                        .position(|(n, _)| *n == name)
                        .unwrap_or_else(|| {
                            times.push((name, [None; 2]));
                            times.len() - 1
                        });
                    times[i].1[side] = ns;
                }
            }
            for (metric, ns) in times {
                let iteration = ns[0].or(ns[1]).unwrap_or_default();
                self.divan.push(DivanBench {
                    metric,
                    added: added_iterations(iteration),
                    sides: ns.map(|t| t.is_some()),
                });
            }
        }
        Ok(())
    }

    fn rounds(
        &self,
        rounds: usize,
        sets: [&Path; 2],
        groups: &[&str],
        allow_missing_baseline: bool,
    ) -> Result<Vec<Metric>> {
        if groups.contains(&"comments") {
            comments::fixture(&self.sandbox, &sets[1].join("dffy"))?;
        }
        let mut runs_on: HashMap<&str, [bool; 2]> = HashMap::new();
        for need in groups.iter().filter_map(|g| needs(g)) {
            let dir = self.sandbox.path("");
            let has = sets.map(|set| (need.has)(&set.join("dffy"), &dir));
            runs_on.insert(need.group, sides(need, has, allow_missing_baseline)?);
        }
        let mut metrics: Vec<Metric> = Vec::new();
        for round in 1..=rounds {
            let order = if round % 2 == 1 { [0, 1] } else { [1, 0] };
            for group in groups {
                for side in order {
                    if runs_on.get(group).is_some_and(|on| !on[side]) {
                        continue;
                    }
                    eprintln!("round {round}/{rounds}: {group}, {}", SIDES[side]);
                    let tag = format!("{round}-{}", SIDES[side]);
                    let mut rows = self.measure(group, sets[side], side, &tag)?;
                    if self.quick {
                        rows = counts_only(rows);
                    }
                    if rows.is_empty() {
                        return Err(format!(
                            "{group}: nothing parsed from {}'s output in {}",
                            sets[side].display(),
                            self.raw.display()
                        ));
                    }
                    for (name, unit, value) in rows {
                        let i = metrics
                            .iter()
                            .position(|m| m.name == name)
                            .unwrap_or_else(|| {
                                metrics.push(Metric {
                                    name,
                                    unit,
                                    values: [Vec::new(), Vec::new()],
                                });
                                metrics.len() - 1
                            });
                        metrics[i].values[side].extend(value);
                    }
                }
            }
        }
        Ok(metrics)
    }

    /// Instructions per iteration of the divan benchmark `metric` in each
    /// set, three measurements a side, alternating which goes first. A
    /// measurement runs the benchmark alone at each of `DIVAN_ITERATIONS`
    /// and divides the difference in instructions by the difference in
    /// iterations, which cancels the fixture's loading and divan's start.
    fn divan_instructions(&self, metric: &str, sets: [&Path; 2]) -> Result<Metric> {
        let mut values = [Vec::new(), Vec::new()];
        // Without a counter the metric has no values and shows as such.
        let rounds = if self.counter { 3 } else { 0 };
        for round in 1..=rounds {
            let order = if round % 2 == 1 { [0, 1] } else { [1, 0] };
            for side in order {
                eprintln!("instructions {round}/3: {metric}, {}", SIDES[side]);
                let mut counts = [0.0; 2];
                for (count, iters) in counts.iter_mut().zip(DIVAN_ITERATIONS) {
                    let tag = format!("instructions-{round}-{}", SIDES[side]);
                    *count = self.divan_run(metric, sets[side], [iters, 1], &tag)?.0;
                }
                let iters = f64::from(DIVAN_ITERATIONS[1] - DIVAN_ITERATIONS[0]);
                values[side].push((counts[1] - counts[0]) / iters);
            }
        }
        Ok(Metric {
            name: format!("{metric} instructions"),
            unit: Unit::Instructions,
            values,
        })
    }

    /// `--quick`'s rows of the divan suite `suite` in `set`, side `side`:
    /// each benchmark's instructions per iteration and its allocations.
    fn divan_counts(&self, suite: &str, set: &Path, side: usize, tag: &str) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for bench in self
            .divan
            .iter()
            .filter(|b| b.metric.split('/').next() == Some(suite))
        {
            if !bench.sides[side] {
                continue;
            }
            let [short, long] = quick_runs(bench.added);
            let (first, _) = self.divan_run(&bench.metric, set, short, tag)?;
            let (n, runs) = self.divan_run(&bench.metric, set, long, tag)?;
            let added = long[0] * (long[1] - short[1]);
            let per_iteration = (n - first) / f64::from(added);
            let name = format!("{} instructions", bench.metric);
            rows.push((name, Unit::Instructions, Some(per_iteration)));
            rows.extend(runs);
        }
        Ok(rows)
    }

    /// The divan benchmark `metric` of `set` run alone for `count` samples of
    /// `size` iterations: the run's instructions, and the benchmark's rows.
    fn divan_run(
        &self,
        metric: &str,
        set: &Path,
        [count, size]: [u32; 2],
        tag: &str,
    ) -> Result<(f64, Vec<Row>)> {
        let (suite, _) = metric.split_once('/').unwrap_or_default();
        // divan's path of the benchmark, which starts with the crate.
        let exact = metric.replace('/', "::");
        let mut cmd = self.command(set.join(suite))?;
        cmd.args(["--bench", "--exact", &exact, "--threads", "1"])
            .args(["--sample-count", &count.to_string()])
            .args(["--sample-size", &size.to_string()])
            .args(["--color", "never", "--bytes-format", "decimal"]);
        let (out, n) = self.run_counted(&mut cmd, tag, &format!("{metric} {count}x{size}"))?;
        let rows = parse_divan(suite, &out)?;
        if !rows.iter().any(|(name, ..)| name == metric) {
            return Err(format!("{}: ran no {exact}", describe(&cmd)));
        }
        Ok((n.ok_or("the instruction counter stopped opening")?, rows))
    }

    /// The sandbox, emptied for one run.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Result<Command> {
        self.sandbox.reset()?;
        Ok(self.sandbox.command(program))
    }

    /// Runs `cmd`, appending its stdout and stderr to the raw files named
    /// after `tag` and `name`.
    fn run(&self, cmd: &mut Command, tag: &str, name: &str) -> Result<(String, String)> {
        let stem = format!("{tag}-{}", name.replace([' ', '/'], "-"));
        let result = crate::output(cmd);
        let (stdout, stderr) = match &result {
            Ok((out, err)) => (out.as_str(), err.as_str()),
            Err(e) => ("", e.as_str()),
        };
        for (ext, text) in [("txt", stdout), ("err", stderr)] {
            if !text.is_empty() {
                self.record(&format!("{stem}.{ext}"), text)?;
            }
        }
        result
    }

    /// Appends `text` to the raw file `name`.
    fn record(&self, name: &str, text: &str) -> Result<()> {
        let path = self.raw.join(name);
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// `run`'s stdout, and the instructions of `cmd` and what it started.
    fn run_counted(
        &self,
        cmd: &mut Command,
        tag: &str,
        name: &str,
    ) -> Result<(String, Option<f64>)> {
        instructions(|| self.run(cmd, tag, name).map(|(out, _)| out))
    }

    /// `group`'s rows for `set`, which is side `side`.
    fn measure(&self, group: &str, set: &Path, side: usize, tag: &str) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        match group {
            "frame" | "engine" if self.quick => rows = self.divan_counts(group, set, side, tag)?,
            "frame" | "engine" => {
                let mut cmd = self.command(set.join(group))?;
                cmd.args(["--bench", "--color", "never", "--bytes-format", "decimal"]);
                rows = parse_divan(group, &self.run(&mut cmd, tag, group)?.0)?;
            }
            "pty" => {
                // first_frame keeps dffy's cache and state under TMPDIR,
                // which the sandbox empties.
                let mut cmd = self.command(set.join("first_frame"))?;
                cmd.arg(set.join("dffy"));
                for (name, value) in median_lines(&self.run(&mut cmd, tag, "first_frame")?.0) {
                    rows.push((format!("pty/{name}"), Unit::Time, Some(nanos(value)?)));
                }
            }
            "count" => {
                let (out, n) = self.run_counted(
                    &mut self.command(set.join("count_pass"))?,
                    tag,
                    "count_pass",
                )?;
                for (_, value) in median_lines(&out) {
                    rows.push(("count_pass".to_owned(), Unit::Time, Some(nanos(value)?)));
                }
                rows.push(("count_pass instructions".to_owned(), Unit::Instructions, n));
                for line in out.lines() {
                    if let Some(n) = line.strip_suffix(" files whose count differs from their diff")
                    {
                        let n = n.parse().map_err(|_| format!("count_pass: {line:?}"))?;
                        rows.push((COUNT_MISMATCH.to_owned(), Unit::Count, Some(n)));
                    }
                }
            }
            // Five runs a round; the round keeps the median. first_1 is the
            // first change found, all the end of the enumeration, in ms from
            // process start.
            "paint" => {
                for mode in ["range", "worktree"] {
                    let mut out = String::new();
                    let mut counts = Vec::new();
                    for _ in 0..5 {
                        let mut cmd = self.command(set.join("first_paint"))?;
                        cmd.arg(&self.sandbox.repo).arg(mode);
                        let name = format!("first_paint {mode}");
                        let (run, n) = self.run_counted(&mut cmd, tag, &name)?;
                        out += &run;
                        counts.extend(n);
                    }
                    for key in ["first_1", "all"] {
                        let mut ms: Vec<f64> = out
                            .split_whitespace()
                            .filter_map(|kv| kv.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
                            .collect();
                        if let Some(ms) = median(&mut ms) {
                            let name = format!("paint/{mode} {key}");
                            rows.push((name, Unit::Time, Some(ms * 1e6)));
                        }
                    }
                    let n = median(&mut counts);
                    rows.push((format!("paint/{mode} instructions"), Unit::Instructions, n));
                }
            }
            "print" => {
                let range = ["v6.0..v6.1", "--paging=never", "--color=always"];
                let plain = [&range[..], &["--no-highlight"]].concat();
                let fair = [&range[..], &["--", "kernel/sched/fair.c"]].concat();
                let status = ["--name-status", "--color=never", "v6.0..v6.1"];
                if self.quick {
                    for (name, extra) in PRINT_SUBSET {
                        let args = [&range[..], extra, &["--"], &SUBSET].concat();
                        rows.push(self.counted(tag, name, set, &args, None)?);
                    }
                    rows.push(self.counted(tag, "print/fair.c", set, &fair, None)?);
                    rows.push(self.counted(tag, "print/name-status", set, &status, None)?);
                    return Ok(rows);
                }
                rows.extend(self.hyperfine(tag, "print/range", 3, 0, set, &range, None)?);
                let name = "print/range no-highlight";
                rows.extend(self.hyperfine(tag, name, 5, 1, set, &plain, None)?);
                rows.extend(self.hyperfine(tag, "print/fair.c", 10, 1, set, &fair, None)?);
                let name_status = "print/name-status";
                rows.extend(self.hyperfine(tag, name_status, 10, 1, set, &status, None)?);
                let rss = format!("{name} peak RSS");
                rows.extend(self.peak_rss(tag, &rss, set, &plain, None)?);
            }
            "pager" => {
                let args = ["--paging=never", "--color=always"];
                let patch = Some(self.patch.as_path());
                if self.quick {
                    rows.push(self.counted(tag, PAGER_SUBSET, set, &args, patch)?);
                    return Ok(rows);
                }
                rows.extend(self.hyperfine(tag, "pager/range", 5, 1, set, &args, patch)?);
                rows.extend(self.peak_rss(tag, "pager/range peak RSS", set, &args, patch)?);
            }
            "comments" => rows = self.comments(set, tag)?,
            _ => return Err(format!("unknown group {group}")),
        }
        Ok(rows)
    }

    /// The comments group's rows for `set`, also kept as raw text.
    fn comments(&self, set: &Path, tag: &str) -> Result<Vec<Row>> {
        let times = comments::measure(&self.sandbox, &set.join("dffy"))?;
        let mut raw = String::new();
        let mut rows = Vec::new();
        for (name, ms) in times {
            let _ = writeln!(raw, "{name}: {ms:.1}ms");
            rows.push((format!("comments/{name}"), Unit::Time, Some(ms * 1e6)));
        }
        self.record(&format!("{tag}-comments.txt"), &raw)?;
        Ok(rows)
    }

    /// hyperfine's median for dffy with `args` and `input` as its stdin, run
    /// without a shell and with dffy's disk cache emptied before every run,
    /// and the instructions of one more run.
    #[expect(clippy::too_many_arguments, reason = "one per hyperfine setting")]
    fn hyperfine(
        &self,
        tag: &str,
        name: &str,
        runs: u32,
        warmup: u32,
        set: &Path,
        args: &[&str],
        input: Option<&Path>,
    ) -> Result<[Row; 2]> {
        let json = self
            .raw
            .join(format!("{tag}-{}.json", name.replace([' ', '/'], "-")));
        let mut cmd = self.command(&self.hyperfine)?;
        cmd.args(["-N", "--style", "none", "--runs", &runs.to_string()])
            .args(["--warmup", &warmup.to_string()])
            .args([
                "--prepare",
                &format!("rm -rf {}", quote(self.sandbox.dffy_cache())),
            ])
            .args(["--command-name", name, "--export-json"])
            .arg(&json);
        if let Some(input) = input {
            cmd.arg("--input").arg(input);
        }
        let dffy = set.join("dffy");
        let words: Vec<String> = std::iter::once(quote(&dffy))
            .chain(args.iter().map(quote))
            .collect();
        cmd.arg(words.join(" "));
        self.run(&mut cmd, tag, name)?;
        let time = Timing::read(&json)?.median * 1e9;
        Ok([
            (name.to_owned(), Unit::Time, Some(time)),
            self.counted(tag, name, set, args, input)?,
        ])
    }

    /// The instructions of one run of dffy with `args` and `input` as its
    /// stdin, as `<name> instructions`.
    fn counted(
        &self,
        tag: &str,
        name: &str,
        set: &Path,
        args: &[&str],
        input: Option<&Path>,
    ) -> Result<Row> {
        // Output to /dev/null as hyperfine's, and stdin /dev/null without
        // an input.
        let counted = format!("{name} instructions");
        let mut cmd = self.command(set.join("dffy"))?;
        cmd.args(args).stdout(Stdio::null());
        if let Some(input) = input {
            cmd.stdin(File::open(input).map_err(|e| format!("{}: {e}", input.display()))?);
        }
        let (_, n) = self.run_counted(&mut cmd, tag, &counted)?;
        Ok((counted, Unit::Instructions, n))
    }

    /// Peak RSS of one run of dffy with `args`, and `input` as its stdin.
    fn peak_rss(
        &self,
        tag: &str,
        name: &str,
        set: &Path,
        args: &[&str],
        input: Option<&Path>,
    ) -> Result<Option<Row>> {
        if !self.time {
            return Ok(None);
        }
        let mut cmd = self.command(TIME)?;
        cmd.arg(time_flag())
            .arg(set.join("dffy"))
            .args(args)
            .stdout(Stdio::null());
        if let Some(input) = input {
            cmd.stdin(File::open(input).map_err(|e| format!("{}: {e}", input.display()))?);
        }
        let (_, stderr) = self.run(&mut cmd, tag, name)?;
        let bytes = peak_rss(&stderr)
            .ok_or_else(|| format!("{}: no peak RSS in its output", describe(&cmd)))?;
        Ok(Some((name.to_owned(), Unit::Bytes, Some(bytes))))
    }
}

/// `name: median value, ...` lines, as the examples print them.
fn median_lines(out: &str) -> impl Iterator<Item = (&str, &str)> {
    out.lines().filter_map(|line| {
        let (name, rest) = line.split_once(": median ")?;
        Some((name, rest.split(',').next()?))
    })
}

/// A number and the unit after it: `121.6 µs`, `506.698042ms`, `87`.
fn quantity(s: &str) -> Result<(f64, &str)> {
    let s = s.trim();
    let end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let value = s[..end]
        .parse()
        .map_err(|_| format!("not a number: {s:?}"))?;
    Ok((value, s[end..].trim()))
}

fn nanos(s: &str) -> Result<f64> {
    let (value, unit) = quantity(s)?;
    let scale = match unit {
        "ns" => 1.0,
        "µs" | "us" => 1e3,
        "ms" => 1e6,
        "s" => 1e9,
        _ => return Err(format!("unknown time unit in {s:?}")),
    };
    Ok(value * scale)
}

fn bytes(s: &str) -> Result<f64> {
    let (value, unit) = quantity(s)?;
    let scale = match unit {
        "B" => 1.0,
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        _ => return Err(format!("unknown byte unit in {s:?}")),
    };
    Ok(value * scale)
}

fn count(s: &str) -> Result<f64> {
    match quantity(s)? {
        (value, "") => Ok(value),
        _ => Err(format!("not a count: {s:?}")),
    }
}

/// A divan bench's table, which has no machine-readable form. Columns are
/// split on `│` from the right, since the tree on the left uses it too:
/// fastest (after the tree and the label), slowest, median, mean, samples,
/// iters. A benchmark is named by its path of tree labels; its time is the
/// median column, and its allocations the mean column of the two lines
/// after `alloc:`. Not the median column: that holds the allocations of the
/// median-time sample, which for a benchmark alternating inputs flips from
/// run to run.
fn parse_divan(suite: &str, out: &str) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    let mut path: Vec<&str> = Vec::new();
    let mut bench = String::new();
    // Lines of an `alloc:` block still to read: the count, then the bytes.
    let mut alloc = 0;
    for line in out.lines() {
        let cols: Vec<&str> = line.rsplitn(6, '│').collect();
        let [_, _, mean, median, _, first] = cols[..] else {
            continue;
        };
        let marker = ["├─ ", "╰─ "]
            .iter()
            .find_map(|m| first.find(m).map(|at| (at, m.len())));
        if let Some((at, len)) = marker {
            alloc = 0;
            // Each level of the tree is three columns wide.
            path.truncate(first[..at].chars().count() / 3);
            let label = first[at + len..].trim();
            if median.trim().is_empty() {
                path.push(label);
                continue;
            }
            // The label ends with the fastest time, `0  92.39 µs`.
            let label = label.rsplitn(3, ' ').nth(2).unwrap_or_default().trim_end();
            path.push(label);
            bench = format!("{suite}/{}", path.join("/"));
            rows.push((bench.clone(), Unit::Time, Some(nanos(median)?)));
            continue;
        }
        match first.trim_start_matches(['│', ' ']).trim_end() {
            "alloc:" => alloc = 2,
            label if label.ends_with(':') => alloc = 0,
            _ if alloc == 2 => {
                rows.push((
                    format!("{bench} alloc count"),
                    Unit::Count,
                    Some(count(mean)?),
                ));
                alloc = 1;
            }
            _ if alloc == 1 => {
                rows.push((
                    format!("{bench} alloc bytes"),
                    Unit::Bytes,
                    Some(bytes(mean)?),
                ));
                alloc = 0;
            }
            _ => {}
        }
    }
    Ok(rows)
}

/// `--quick`'s rows of one run: instruction counts, allocations and
/// `count_pass`'s mismatches, with no timings.
fn counts_only(mut rows: Vec<Row>) -> Vec<Row> {
    rows.retain(|(_, unit, _)| *unit != Unit::Time);
    rows
}

/// The `--sample-count` and `--sample-size` of `--quick`'s two runs of a
/// divan benchmark, the second `added` iterations longer: the same number
/// of samples, so divan's work per sample (timer reads, its sample and
/// allocation records, sorting the samples) is the same in both and cancels
/// in the difference, and [`ADDED_PER_SAMPLE`] more iterations in each.
fn quick_runs(added: u32) -> [[u32; 2]; 2] {
    let count = (added / ADDED_PER_SAMPLE).max(1);
    [
        [count, FIRST_ITERATIONS],
        [count, FIRST_ITERATIONS + added.div_ceil(count)],
    ]
}

/// The iterations `--quick`'s longer run adds to each sample, at most. A
/// sample holds every iteration's input and output until it ends, and
/// `frame/prepare_file/table` holds about 3.5 MB an iteration, so a sample
/// of 10 holds about 35 MB.
const ADDED_PER_SAMPLE: u32 = 8;

/// Iterations the second counted run of a divan benchmark adds to the
/// first: the power of two nearest to [`ADDED_NS`] of them, from 2 to 2^18,
/// so the variation in each run's instructions, mostly its setup's, stays
/// small against the difference. A power of two: even, so a benchmark
/// alternating two inputs counts both alike, and unchanged by small
/// differences in timing from one invocation to the next. The cap holds
/// divan's per-sample records to 2^15 samples for a benchmark of a few
/// nanoseconds.
fn added_iterations(iteration_ns: f64) -> u32 {
    let exponent = (ADDED_NS / iteration_ns.max(1.0)).log2().round();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 1..=18"
    )]
    let exponent = exponent.clamp(1.0, 18.0) as u32;
    1 << exponent
}

/// The time, in nanoseconds, of the iterations `--quick`'s longer run of a
/// divan benchmark adds.
const ADDED_NS: f64 = 200e6;

/// The instruction count whose values on one side are furthest apart,
/// relative to their median, and that distance.
fn largest_spread(metrics: &[Metric]) -> Option<(f64, &str)> {
    metrics
        .iter()
        .filter(|m| m.unit == Unit::Instructions)
        .flat_map(|m| m.values.iter().map(move |v| (v, m.name.as_str())))
        .filter_map(|(values, name)| {
            let mid = median(&mut values.clone()).filter(|&mid| mid > 0.0)?;
            let (lo, hi) = values
                .iter()
                .fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
            Some(((hi - lo) / mid, name))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
}

/// The largest difference between the two sides of one round.
fn band(m: &Metric) -> f64 {
    m.values[0]
        .iter()
        .zip(&m.values[1])
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max)
}

/// The bands in `path`, none when it's missing.
pub(crate) fn read_bands(path: &Path) -> HashMap<String, f64> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split('\t');
            Some((fields.next()?.to_owned(), fields.next()?.parse().ok()?))
        })
        .collect()
}

/// Writes `bands`, each a metric, its band and unit, into `path` under the
/// comment `header`, keeping the bands of metrics this run didn't measure,
/// so a run of some groups leaves the others' bands.
fn write_bands(path: &Path, header: &str, bands: &[(String, f64, &str)]) -> Result<()> {
    let measured = |name: &str| bands.iter().any(|(n, ..)| n == name);
    let mut out = format!("{header}\n");
    if let Ok(old) = fs::read_to_string(path) {
        for line in old.lines().filter(|l| !l.starts_with('#')) {
            if !measured(line.split('\t').next().unwrap_or_default()) {
                out += line;
                out.push('\n');
            }
        }
    }
    for (name, band, unit) in bands {
        let _ = writeln!(out, "{name}\t{band}\t{unit}");
    }
    fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
}

fn write_noise(path: &Path, metrics: &[Metric], rounds: usize) -> Result<()> {
    let header = format!(
        "# cargo xtask bench-noise ({rounds} rounds, {}): metric, the largest difference between A and B of one round with A and B the same set, unit",
        now()
    );
    let bands: Vec<_> = metrics
        .iter()
        .filter(|m| m.name != COUNT_MISMATCH && !m.values[0].is_empty())
        .map(|m| (m.name.clone(), band(m), m.unit.symbol()))
        .collect();
    write_bands(path, &header, &bands)
}

/// Builds `count` sets of the checkout, or of `worktree`, each with its own
/// code layout, into `base/layouts/<k>`, measures every set after the first
/// against the first, and writes each metric's layout band to `out`: the
/// largest difference of a set's median from the first set's, relative to
/// the first set's.
pub(crate) fn measure_layouts(
    base: &Path,
    count: usize,
    rounds: usize,
    groups: &[&str],
    worktree: Option<&Path>,
    out: &Path,
) -> Result<()> {
    let sets: Vec<PathBuf> = (0..count)
        .map(|k| base.join("layouts").join(k.to_string()))
        .collect();
    for (k, set) in sets.iter().enumerate() {
        snapshot(set, Some(k + 1), worktree)?;
    }
    let mut runner = Runner::new(Sandbox::new(bench_repo()?, false)?, groups, false)?;
    let raw = runner.raw.clone();
    let mut bands: Vec<(String, f64, &str)> = Vec::new();
    for (k, set) in sets.iter().enumerate().skip(1) {
        runner.raw = raw.join(format!("layout-{k}"));
        fs::create_dir_all(&runner.raw).map_err(|e| format!("{}: {e}", runner.raw.display()))?;
        for m in runner.rounds(rounds, [&sets[0], set], groups, false)? {
            let ((Some(a), Some(b)), false) = (medians(&m), m.name == COUNT_MISMATCH) else {
                continue;
            };
            let band = if a > 0.0 { (b - a).abs() / a } else { 0.0 };
            match bands.iter_mut().find(|(name, ..)| *name == m.name) {
                Some(entry) => entry.1 = entry.1.max(band),
                None => bands.push((m.name, band, "rel")),
            }
        }
    }
    let path = out;
    let commit = fs::read_to_string(sets[0].join("REV")).unwrap_or_default();
    let header = format!(
        "# cargo xtask bench-layouts (commit {}, {count} layouts, {rounds} rounds, {}): metric, the largest difference of a layout's median from layout 0's, as a fraction of layout 0's, rel",
        crate::results::short(commit.trim()),
        now()
    );
    write_bands(path, &header, &bands)?;
    let mut summary = format!(
        "# dffy layout bands\n\n- {count} layouts of {}; rounds: {rounds}; groups: {}\n\n| Metric | Layout band |\n|---|---|\n",
        base.display(),
        groups.join(", ")
    );
    for (name, band, _) in &bands {
        let _ = writeln!(summary, "| {name} | {:.2}% |", band * 100.0);
    }
    let _ = write!(summary, "\nWrote the layout bands to {}.\n", path.display());
    print!("{summary}");
    let summary_path = raw.join("summary.md");
    fs::write(&summary_path, summary).map_err(|e| format!("{}: {e}", summary_path.display()))?;
    eprintln!("raw results: {}", raw.display());
    Ok(())
}

/// The one-sided sign test of B slower than A in `k` of `n` rounds: the
/// p-value P(X ≥ k), X binomial with `n` trials of probability 1/2, as the
/// number of outcomes at least that extreme and the number of outcomes, 2^n.
fn sign_test(k: usize, n: usize) -> (u128, u128) {
    let (mut tail, mut c) = (0, 1u128);
    for i in 0..=n {
        if i >= k {
            tail += c;
        }
        c = c * (n - i) as u128 / (i + 1) as u128;
    }
    (tail, 1 << n)
}

/// Whether a timing or peak-RSS metric slowed down: B's median more than
/// `threshold`, a fraction, above A's, and B slower in `k` of `n` rounds
/// with a sign test p < 0.05.
fn slower(a: f64, b: f64, k: usize, n: usize, threshold: f64) -> bool {
    let (tail, outcomes) = sign_test(k, n);
    b > a * (1.0 + threshold) && tail * 20 < outcomes
}

/// Whether an instruction count grew: B's median more than 0.5% above A's.
fn more_instructions(a: f64, b: f64) -> bool {
    b > a * 1.005
}

/// The medians of A and B.
fn medians(m: &Metric) -> (Option<f64>, Option<f64>) {
    (
        median(&mut m.values[0].clone()),
        median(&mut m.values[1].clone()),
    )
}

/// The rounds B was slower than A in, and the rounds both sides have.
fn slower_rounds(metric: &Metric) -> (usize, usize) {
    let [a, b] = &metric.values;
    let slower = a.iter().zip(b).filter(|(a, b)| b > a).count();
    (slower, a.len().min(b.len()))
}

/// How far above A's median a timing or peak-RSS metric's median in B may
/// go, as a fraction: 1%, or the metric's band in `layout` when larger.
fn threshold(metric: &str, layout: &HashMap<String, f64>) -> f64 {
    layout.get(metric).copied().unwrap_or(0.0).max(0.01)
}

/// Whether a timing or peak-RSS metric fails the timing rule.
fn timing_fails(metric: &Metric, layout: &HashMap<String, f64>) -> bool {
    let (Some(a), Some(b)) = medians(metric) else {
        return false;
    };
    let (k, n) = slower_rounds(metric);
    slower(a, b, k, n, threshold(&metric.name, layout))
}

/// The Result cell of a timing or peak-RSS metric: a failure when its
/// instruction count fails, else the timing rule's, noting the threshold
/// when the metric's layout band raised it above 1% and the instruction
/// count when there is one.
fn timing_result(metric: &Metric, metrics: &[Metric], layout: &HashMap<String, f64>) -> String {
    let counted = format!("{} instructions", metric.name);
    let more = match metrics.iter().find(|i| i.name == counted).map(medians) {
        Some((Some(a), Some(b))) => Some(more_instructions(a, b)),
        _ => None,
    };
    if more == Some(true) {
        return "FAIL (instructions)".to_owned();
    }
    let threshold = threshold(&metric.name, layout);
    let mut notes = Vec::new();
    if threshold > 0.01 {
        notes.push(format!("threshold {:.1}%", threshold * 100.0));
    }
    if more == Some(false) {
        notes.push("instructions flat".to_owned());
    }
    let result = if timing_fails(metric, layout) {
        "FAIL"
    } else {
        "PASS"
    };
    if notes.is_empty() {
        result.to_owned()
    } else {
        format!("{result} ({})", notes.join(", "))
    }
}

/// The markdown table, whether every metric passed, and each metric's
/// Result cell. `bands` are from `noise.tsv`, `layout` from `layout.tsv`.
/// `measuring` is A compared with itself: the table adds the bands being
/// measured, and a metric the rule fails is a false alarm, which doesn't
/// fail the run.
fn table(
    metrics: &[Metric],
    bands: &HashMap<String, f64>,
    layout: &HashMap<String, f64>,
    measuring: bool,
) -> (String, bool, Vec<String>) {
    let mut verdicts = Vec::new();
    let mut out = if measuring {
        "| Metric | A | B | Delta | Slower rounds | Band | Result |\n|---|---|---|---|---|---|---|\n"
    } else {
        "| Metric | A | B | Delta | Slower rounds | Result |\n|---|---|---|---|---|---|\n"
    }
    .to_owned();
    let (mut failed, mut false_alarms) = (0, 0);
    for m in metrics {
        let (a, b) = medians(m);
        let delta = match (a, b) {
            (Some(a), Some(b)) if a > 0.0 => format!("{:+.1}%", (b - a) / a * 100.0),
            (Some(_), Some(b)) if b > 0.0 => "—".to_owned(),
            (Some(_), Some(_)) => "+0.0%".to_owned(),
            _ => "—".to_owned(),
        };
        let (k, n) = slower_rounds(m);
        let result = if m.name == COUNT_MISMATCH {
            let sides = if measuring { 0..2 } else { 1..2 };
            if m.values[sides].iter().flatten().any(|&n| n > 0.0) {
                "FAIL"
            } else {
                "PASS"
            }
            .to_owned()
        } else {
            match (a, b) {
                (None, None) => "not measured".to_owned(),
                (None, _) => "only in B".to_owned(),
                (_, None) => "FAIL (missing in B)".to_owned(),
                (Some(a), Some(b)) if m.unit == Unit::Instructions => if more_instructions(a, b) {
                    "FAIL"
                } else {
                    "PASS"
                }
                .to_owned(),
                (Some(a), Some(b))
                    if m.name.ends_with(" alloc count") || m.name.ends_with(" alloc bytes") =>
                {
                    let band = bands.get(&m.name).copied().unwrap_or(0.0);
                    let result = if b > a + band { "FAIL" } else { "PASS" };
                    if band > 0.0 && !measuring {
                        format!("{result} (band {})", show(band, m.unit))
                    } else {
                        result.to_owned()
                    }
                }
                (Some(_), Some(_)) => timing_result(m, metrics, layout),
            }
        };
        let result = if measuring && m.name != COUNT_MISMATCH && result.starts_with("FAIL") {
            false_alarms += 1;
            "false alarm".to_owned()
        } else {
            result
        };
        if result.starts_with("FAIL") {
            failed += 1;
        }
        let rounds = if n == 0 {
            "—".to_owned()
        } else {
            format!("{k}/{n}")
        };
        let band_cell = match (measuring, m.name == COUNT_MISMATCH || n == 0) {
            (false, _) => String::new(),
            (true, true) => " — |".to_owned(),
            (true, false) => format!(" {} |", show(band(m), m.unit)),
        };
        let cell = |v: Option<f64>| v.map_or_else(|| "—".to_owned(), |v| show(v, m.unit));
        let _ = writeln!(
            out,
            "| {} | {} | {} | {delta} | {rounds} |{band_cell} {result} |",
            m.name,
            cell(a),
            cell(b)
        );
        verdicts.push(result);
    }
    if measuring {
        let _ = write!(
            out,
            "\nFalse alarms, metrics the rule fails with A against itself: {false_alarms} of {}.\n",
            metrics.len()
        );
    }
    let _ = write!(out, "\nFailed: {failed} of {} metrics.\n", metrics.len());
    (out, failed == 0, verdicts)
}

fn show(v: f64, unit: Unit) -> String {
    crate::results::show(v, unit.name())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A group compares A with B, so a baseline without what it needs
    /// fails the run unless `--allow-missing-baseline` says to run it on B
    /// alone; a checkout without it always fails. The comments group needs
    /// a dffy that opens a comments file.
    #[test]
    fn a_baseline_without_what_a_group_needs_fails_it_unless_allowed() {
        let need = needs("comments").expect("the comments group needs comments");
        assert_eq!(sides(need, [true, true], false), Ok([true, true]));
        let refused = sides(need, [false, true], false).unwrap_err();
        assert!(refused.contains("--allow-missing-baseline"), "{refused}");
        assert!(refused.contains("open a comments file"), "{refused}");
        assert_eq!(sides(need, [false, true], true), Ok([false, true]));
        assert!(sides(need, [true, false], true).is_err());
        assert!(needs("frame").is_none());
    }

    #[test]
    fn divan_names_times_and_mean_allocations() {
        let out = "\
frame                       fastest       │ slowest       │ median        │ mean          │ samples │ iters
├─ full_frame                             │               │               │               │         │
│  ├─ fair.c renderer idle  119.8 µs      │ 179.1 µs      │ 122.6 µs      │ 125.6 µs      │ 100     │ 100
│  │                        max alloc:    │               │               │               │         │
│  │                          44          │ 44            │ 44            │ 44            │         │
│  │                          1.567 KB    │ 1.567 KB      │ 1.567 KB      │ 1.567 KB      │         │
│  │                        alloc:        │               │               │               │         │
│  │                          96          │ 95            │ 95.5          │ 95.5          │         │
│  │                          2.792 KB    │ 2.782 KB      │ 2.787 KB      │ 2.788 KB      │         │
│  │                        grow:         │               │               │               │         │
│  │                          21          │ 21            │ 21            │ 21            │         │
│  │                          1.04 KB     │ 1.04 KB       │ 1.04 KB       │ 1.04 KB       │         │
│  ╰─ 0                     92.39 µs      │ 125.8 µs      │ 101 µs        │ 1 ms          │ 100     │ 100
╰─ tree_next_file           109.4 µs      │ 190.5 µs      │ 2 s           │ 146.2 µs      │ 100     │ 100
                            alloc:        │               │               │               │         │
                              439         │ 471           │ 471           │ 455           │         │
                              12.96 KB    │ 13.42 KB      │ 13.42 KB      │ 776.5 B       │         │
";
        let rows = parse_divan("frame", out).unwrap();
        let expected = [
            (
                "frame/full_frame/fair.c renderer idle",
                Unit::Time,
                122_600.0,
            ),
            (
                "frame/full_frame/fair.c renderer idle alloc count",
                Unit::Count,
                95.5,
            ),
            (
                "frame/full_frame/fair.c renderer idle alloc bytes",
                Unit::Bytes,
                2788.0,
            ),
            ("frame/full_frame/0", Unit::Time, 101_000.0),
            ("frame/tree_next_file", Unit::Time, 2e9),
            ("frame/tree_next_file alloc count", Unit::Count, 455.0),
            ("frame/tree_next_file alloc bytes", Unit::Bytes, 776.5),
        ];
        assert_eq!(rows.len(), expected.len(), "{rows:?}");
        for ((name, unit, value), (want_name, want_unit, want)) in rows.iter().zip(expected) {
            assert_eq!((name.as_str(), *unit), (want_name, want_unit));
            let value = value.unwrap_or(f64::NAN);
            assert!((value - want).abs() < 1e-6, "{name}: {value} != {want}");
        }
    }

    #[test]
    fn durations_in_every_unit() {
        let parsed: Vec<f64> = ["7.2ms", "506.698042ms", "129 µs", "1.5s", "12ns"]
            .iter()
            .map(|s| nanos(s).unwrap())
            .collect();
        let expected = [7.2e6, 506_698_042.0, 129_000.0, 1.5e9, 12.0];
        for (got, want) in parsed.iter().zip(expected) {
            assert!((got - want).abs() < 1e-3, "{got} != {want}");
        }
    }

    #[test]
    fn sign_test_p_values() {
        // 11/1024 ≈ 0.0107, 56/1024 ≈ 0.0547, 1/32 = 0.03125.
        assert_eq!(sign_test(9, 10), (11, 1024));
        assert_eq!(sign_test(8, 10), (56, 1024));
        assert_eq!(sign_test(5, 5), (1, 32));
        // The largest --rounds: every coefficient fits, and they sum to 2^n.
        assert_eq!(sign_test(0, 100), (1 << 100, 1 << 100));
    }

    #[test]
    fn timing_fails_on_a_consistent_slowdown_over_one_percent() {
        assert!(!slower(100.0, 102.0, 8, 10, 0.01));
        assert!(slower(100.0, 102.0, 9, 10, 0.01));
        assert!(!slower(100.0, 100.5, 10, 10, 0.01));
    }

    fn metric(name: &str, unit: Unit, a: &[f64], b: &[f64]) -> Metric {
        Metric {
            name: name.to_owned(),
            unit,
            values: [a.to_vec(), b.to_vec()],
        }
    }

    /// The Result cell of each row, in order.
    fn results(metrics: &[Metric], layout: &[(&str, f64)]) -> Vec<String> {
        let layout = layout.iter().map(|&(n, b)| (n.to_owned(), b)).collect();
        let (out, ..) = table(metrics, &HashMap::new(), &layout, false);
        out.lines()
            .skip(2)
            .take(metrics.len())
            .map(|row| row.rsplit('|').nth(1).unwrap_or_default().trim().to_owned())
            .collect()
    }

    #[test]
    fn instructions_fail_over_half_a_percent_with_no_sign_test() {
        let metrics = [
            metric(
                "a instructions",
                Unit::Instructions,
                &[1e9; 3],
                &[1.004e9; 3],
            ),
            metric(
                "b instructions",
                Unit::Instructions,
                &[1e9; 3],
                &[1.006e9; 3],
            ),
        ];
        assert_eq!(results(&metrics, &[]), ["PASS", "FAIL"]);
    }

    #[test]
    fn instructions_without_a_counter_show_no_values_and_pass() {
        let metrics = [metric("a instructions", Unit::Instructions, &[], &[])];
        let (out, pass, _) = table(&metrics, &HashMap::new(), &HashMap::new(), false);
        assert!(pass);
        assert!(
            out.contains("| a instructions | — | — | — | — | not measured |"),
            "{out}"
        );
    }

    #[test]
    fn timing_threshold_is_the_layout_band_when_larger() {
        let slow = [100.0; 10];
        let slower = [103.0; 10];
        let metrics = [
            metric("a", Unit::Time, &slow, &slower),
            metric("b", Unit::Time, &slow, &slower),
        ];
        assert_eq!(
            results(&metrics, &[("a", 0.05), ("b", 0.005)]),
            ["PASS (threshold 5.0%)", "FAIL"]
        );
    }

    #[test]
    fn timing_fails_with_its_instruction_count() {
        let metrics = [
            metric("a", Unit::Time, &[100.0; 10], &[100.0; 10]),
            metric(
                "a instructions",
                Unit::Instructions,
                &[1e9; 10],
                &[1.01e9; 10],
            ),
        ];
        assert_eq!(results(&metrics, &[]), ["FAIL (instructions)", "FAIL"]);
    }

    #[test]
    fn timing_with_flat_instructions_keeps_its_timing_rule() {
        let metrics = [
            metric("a", Unit::Time, &[100.0; 10], &[103.0; 10]),
            metric(
                "a instructions",
                Unit::Instructions,
                &[1e9; 10],
                &[1.001e9; 10],
            ),
            metric("b", Unit::Time, &[100.0; 10], &[103.0; 10]),
            metric(
                "b instructions",
                Unit::Instructions,
                &[1e9; 10],
                &[0.99e9; 10],
            ),
        ];
        assert_eq!(
            results(&metrics, &[("b", 0.05)]),
            [
                "FAIL (instructions flat)",
                "PASS",
                "PASS (threshold 5.0%, instructions flat)",
                "PASS"
            ]
        );
    }

    #[test]
    fn quick_keeps_counts_and_drops_timings() {
        let rows = vec![
            ("count_pass".to_owned(), Unit::Time, Some(1.0)),
            (
                "count_pass instructions".to_owned(),
                Unit::Instructions,
                Some(1.0),
            ),
            (COUNT_MISMATCH.to_owned(), Unit::Count, Some(0.0)),
            ("frame/a alloc count".to_owned(), Unit::Count, Some(1.0)),
            ("frame/a alloc bytes".to_owned(), Unit::Bytes, Some(1.0)),
        ];
        let kept: Vec<String> = counts_only(rows).into_iter().map(|r| r.0).collect();
        assert_eq!(
            kept,
            [
                "count_pass instructions",
                COUNT_MISMATCH,
                "frame/a alloc count",
                "frame/a alloc bytes"
            ]
        );
    }

    #[test]
    fn quick_fails_on_instructions_over_half_a_percent_or_any_new_allocation() {
        let metrics = [
            metric(
                "frame/a instructions",
                Unit::Instructions,
                &[1e6; 3],
                &[1.004e6; 3],
            ),
            metric(
                "frame/b instructions",
                Unit::Instructions,
                &[1e6; 3],
                &[1.006e6; 3],
            ),
            metric("frame/a alloc count", Unit::Count, &[87.0; 3], &[88.0; 3]),
            metric("frame/b alloc count", Unit::Count, &[87.0; 3], &[88.0; 3]),
            metric(
                "frame/a alloc bytes",
                Unit::Bytes,
                &[2488.0; 3],
                &[2488.0; 3],
            ),
        ];
        let bands = HashMap::from([("frame/b alloc count".to_owned(), 1.0)]);
        let (out, pass, _) = table(&metrics, &bands, &HashMap::new(), false);
        assert!(!pass);
        let results: Vec<&str> = out
            .lines()
            .skip(2)
            .take(metrics.len())
            .map(|row| row.rsplit('|').nth(1).unwrap_or_default().trim())
            .collect();
        assert_eq!(results, ["PASS", "FAIL", "FAIL", "PASS (band 1)", "PASS"]);
    }

    #[test]
    fn quick_names_its_subset_runs_apart_from_the_whole_range() {
        let full = ["print/range", "print/range no-highlight", "pager/range"];
        let quick = PRINT_SUBSET.map(|(name, _)| name);
        for name in quick.iter().chain([&PAGER_SUBSET]) {
            assert!(name.contains("/subset"), "{name}");
            assert!(full.iter().all(|f| !name.starts_with(f)), "{name}");
        }
    }

    #[test]
    fn added_iterations_take_about_200ms() {
        // 100µs per iteration: 2000 wanted, 2048 the nearest power of two.
        assert_eq!(added_iterations(100e3), 2048);
        // 150µs: 1333 wanted, nearer 1024 than 2048.
        assert_eq!(added_iterations(150e3), 1024);
        // Slower than 200ms: the fewest, still even.
        assert_eq!(added_iterations(150e6), 2);
        // A few nanoseconds: capped.
        assert_eq!(added_iterations(5.0), 1 << 18);
    }

    #[test]
    fn quick_runs_differ_only_in_iterations_per_sample() {
        for iteration_ns in [1.6e3, 100e3, 150e6] {
            let added = added_iterations(iteration_ns);
            let [[short_count, short_size], [long_count, long_size]] = quick_runs(added);
            assert_eq!(short_count, long_count);
            assert_eq!(long_count * (long_size - short_size), added);
            assert!(long_size - short_size <= ADDED_PER_SAMPLE);
            // Even, so a benchmark alternating two inputs runs both alike.
            assert!(short_size % 2 == 0 && long_size % 2 == 0);
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn quick_off_linux_says_it_needs_linux() {
        let args = ["base".to_owned(), "--quick".to_owned()];
        let err = main("bench-ab", &args).unwrap_err();
        assert!(err.contains("Linux"), "{err}");
    }
}
