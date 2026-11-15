//! The performance gate: `bench-baseline`, `bench-ab` and `bench-noise`.
//!
//! A set is a directory of release executables: `dffy`, the examples
//! `count_pass`, `first_frame` and `first_paint`, and the divan benches
//! `frame` and `engine`, with the commit in `REV`. Two sets, A and B, are
//! measured round by round, alternating which goes first. A metric's value
//! per side is
//! the median of its per-round values, and B passes when it is at most A's
//! plus the band in A's `noise.tsv`: the largest difference between the two
//! sides of one round when A was compared with itself.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{
    ROOT, Result, Sandbox, TIME, Timing, USAGE, bench_repo, date, describe, expand_tilde, median,
    now, peak_rss, quote, time_flag, which,
};

const GROUPS: [&str; 7] = ["frame", "engine", "pty", "count", "paint", "print", "pager"];

/// A set's executables by cargo target kind and name. cargo also builds
/// test harnesses for the `dffy` lib and bin when it builds the benches, so
/// a match takes kind and name, and bin and example come only from the
/// build, bench only from the bench build.
const EXECUTABLES: [(&str, &str); 6] = [
    ("bin", "dffy"),
    ("example", "count_pass"),
    ("example", "first_frame"),
    ("example", "first_paint"),
    ("bench", "frame"),
    ("bench", "engine"),
];

const SIDES: [&str; 2] = ["A", "B"];

/// `count_pass`'s files whose count differs from their diff: any in B fails,
/// whatever the noise.
const COUNT_MISMATCH: &str = "count_pass differing files";

pub(crate) fn main(cmd: &str, args: &[String]) -> Result<bool> {
    let mut dir = None;
    let mut rounds = 5;
    let mut groups = GROUPS.to_vec();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--rounds" if cmd != "bench-baseline" => {
                rounds = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .filter(|&n| n > 0)
                    .ok_or("--rounds takes a positive number")?;
            }
            "--only" if cmd != "bench-baseline" => {
                let only: Vec<&str> = args.next().ok_or(USAGE)?.split(',').collect();
                if let Some(bad) = only.iter().find(|g| !GROUPS.contains(g)) {
                    return Err(format!(
                        "unknown group {bad}; groups are {}",
                        GROUPS.join(", ")
                    ));
                }
                groups.retain(|g| only.contains(g));
            }
            _ if dir.is_none() && !arg.starts_with('-') => dir = Some(expand_tilde(arg)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let dir = dir.ok_or(USAGE)?;
    let noise = match cmd {
        "bench-baseline" => {
            snapshot(&dir)?;
            return Ok(true);
        }
        "bench-ab" => false,
        "bench-noise" => true,
        _ => return Err(USAGE.to_owned()),
    };

    let base = fs::canonicalize(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let runner = Runner::new(Sandbox::new(bench_repo()?, false)?, &groups)?;
    let candidate = if noise {
        base.clone()
    } else {
        let set = runner.sandbox.path("B");
        snapshot(&set)?;
        set
    };
    for set in [&base, &candidate] {
        for (_, name) in EXECUTABLES {
            if !set.join(name).is_file() {
                return Err(format!(
                    "{}: no {name}; build the set with cargo xtask bench-baseline",
                    set.display()
                ));
            }
        }
    }

    let metrics = runner.rounds(rounds, [&base, &candidate], &groups)?;
    let noise_file = base.join("noise.tsv");
    let mut summary = header([&base, &candidate], rounds, &groups);
    let pass = if noise {
        let (table, pass) = table(&metrics, &Gate::Measuring);
        summary += &table;
        write_noise(&noise_file, &metrics, rounds)?;
        let _ = write!(
            summary,
            "\nWrote the bands of {} metrics to {}.\n",
            metrics.len(),
            noise_file.display()
        );
        pass
    } else {
        let bands = read_noise(&noise_file);
        let gate = bands.as_ref().map_or(Gate::Uncalibrated, Gate::Bands);
        let (table, pass) = table(&metrics, &gate);
        summary += &table;
        if bands.is_none() {
            let _ = write!(
                summary,
                "\nWarning: {} is missing, so the gate is uncalibrated and only count_pass mismatches fail. Measure the noise with `cargo xtask bench-noise {}` on a quiet machine.\n",
                noise_file.display(),
                base.display()
            );
        }
        pass
    };
    print!("{summary}");
    let summary_path = runner.raw.join("summary.md");
    fs::write(&summary_path, summary).map_err(|e| format!("{}: {e}", summary_path.display()))?;
    eprintln!("raw results: {}", runner.raw.display());
    Ok(pass)
}

fn header(sets: [&Path; 2], rounds: usize, groups: &[&str]) -> String {
    let mut out = "# dffy A/B benchmark\n\n".to_owned();
    for (side, set) in SIDES.iter().zip(sets) {
        let rev = fs::read_to_string(set.join("REV")).map_or_else(
            |_| "no REV".to_owned(),
            |r| r.trim().chars().take(12).collect(),
        );
        let _ = writeln!(out, "- {side}: {} ({rev})", set.display());
    }
    let _ = write!(
        out,
        "- Rounds: {rounds}; groups: {}\n- {} {}, {}\n\n",
        groups.join(", "),
        std::env::consts::OS,
        std::env::consts::ARCH,
        now(),
    );
    out
}

/// Builds the current tree and copies its executables into `dest`, with
/// the commit in `dest/REV`.
fn snapshot(dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut found = artifacts(
        &["build", "--release", "--workspace", "--examples", "--bins"],
        &["bin", "example"],
    )?;
    found.extend(artifacts(
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
            .current_dir(ROOT),
    )
    .map_or_else(|_| "unknown\n".to_owned(), |(rev, _)| rev);
    fs::write(dest.join("REV"), &rev).map_err(|e| format!("{}: {e}", dest.display()))?;
    eprintln!("{}: the set at {}", dest.display(), rev.trim());
    Ok(())
}

/// The set's executables of the given kinds that `cargo args` built. The
/// paths come from cargo's messages: target/release/deps keeps earlier
/// builds' executables.
fn artifacts(args: &[&str], kinds: &[&str]) -> Result<Vec<(&'static str, PathBuf)>> {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args(args)
        .arg("--message-format=json-render-diagnostics")
        .current_dir(ROOT)
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
}

impl Unit {
    fn symbol(self) -> &'static str {
        match self {
            Self::Time => "ns",
            Self::Bytes => "B",
            Self::Count => "n",
        }
    }
}

type Row = (String, Unit, f64);

struct Metric {
    name: String,
    unit: Unit,
    /// Per side, one value per round.
    values: [Vec<f64>; 2],
}

struct Runner {
    sandbox: Sandbox,
    /// This run's directory under ~/.cache/dffy-bench/runs.
    raw: PathBuf,
    /// The range's patch for the pager group, written once per run.
    patch: PathBuf,
    hyperfine: PathBuf,
    time: bool,
}

impl Runner {
    fn new(sandbox: Sandbox, groups: &[&str]) -> Result<Self> {
        let raw = expand_tilde("~/.cache/dffy-bench/runs").join(date(&["+%Y%m%d-%H%M%S"]));
        fs::create_dir_all(&raw).map_err(|e| format!("{}: {e}", raw.display()))?;
        let needs = |g: &str| groups.contains(&g);
        let hyperfine = which("hyperfine").unwrap_or_default();
        if (needs("print") || needs("pager")) && hyperfine.as_os_str().is_empty() {
            return Err("hyperfine not found, needed for print and pager".to_owned());
        }
        let time = Path::new(TIME).exists();
        if !time {
            eprintln!("warning: {TIME} not found, peak RSS is skipped");
        }
        let patch = sandbox.path("range.patch");
        if needs("pager") {
            let out = File::create(&patch).map_err(|e| format!("{}: {e}", patch.display()))?;
            crate::output(
                sandbox
                    .command("git")
                    .args(["diff", "--no-ext-diff", "--color=always", "v6.0", "v6.1"])
                    .stdout(out),
            )?;
        }
        Ok(Self {
            sandbox,
            raw,
            patch,
            hyperfine,
            time,
        })
    }

    fn rounds(&self, rounds: usize, sets: [&Path; 2], groups: &[&str]) -> Result<Vec<Metric>> {
        let mut metrics: Vec<Metric> = Vec::new();
        for round in 1..=rounds {
            let order = if round % 2 == 1 { [0, 1] } else { [1, 0] };
            for group in groups {
                for side in order {
                    eprintln!("round {round}/{rounds}: {group}, {}", SIDES[side]);
                    let rows =
                        self.measure(group, sets[side], &format!("{round}-{}", SIDES[side]))?;
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
                        metrics[i].values[side].push(value);
                    }
                }
            }
        }
        Ok(metrics)
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
                let path = self.raw.join(format!("{stem}.{ext}"));
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .and_then(|mut f| f.write_all(text.as_bytes()))
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
        result
    }

    fn measure(&self, group: &str, set: &Path, tag: &str) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        match group {
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
                    rows.push((format!("pty/{name}"), Unit::Time, nanos(value)?));
                }
            }
            "count" => {
                let (out, _) = self.run(
                    &mut self.command(set.join("count_pass"))?,
                    tag,
                    "count_pass",
                )?;
                for (_, value) in median_lines(&out) {
                    rows.push(("count_pass".to_owned(), Unit::Time, nanos(value)?));
                }
                for line in out.lines() {
                    if let Some(n) = line.strip_suffix(" files whose count differs from their diff")
                    {
                        let n = n.parse().map_err(|_| format!("count_pass: {line:?}"))?;
                        rows.push((COUNT_MISMATCH.to_owned(), Unit::Count, n));
                    }
                }
            }
            // Five runs a round; the round keeps the median. first_1 is the
            // first change found, all the end of the enumeration, in ms from
            // process start.
            "paint" => {
                for mode in ["range", "worktree"] {
                    let mut out = String::new();
                    for _ in 0..5 {
                        let mut cmd = self.command(set.join("first_paint"))?;
                        cmd.arg(&self.sandbox.repo).arg(mode);
                        out += &self.run(&mut cmd, tag, &format!("first_paint {mode}"))?.0;
                    }
                    for key in ["first_1", "all"] {
                        let mut ms: Vec<f64> = out
                            .split_whitespace()
                            .filter_map(|kv| kv.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
                            .collect();
                        if let Some(ms) = median(&mut ms) {
                            rows.push((format!("paint/{mode} {key}"), Unit::Time, ms * 1e6));
                        }
                    }
                }
            }
            "print" => {
                let dffy = quote(set.join("dffy"));
                let range = format!("{dffy} v6.0..v6.1 --paging=never --color=always");
                rows.push(self.hyperfine(tag, "print/range", 3, 0, None, &range)?);
                let plain = format!("{range} --no-highlight");
                rows.push(self.hyperfine(tag, "print/range no-highlight", 5, 1, None, &plain)?);
                let fair = format!("{range} -- kernel/sched/fair.c");
                rows.push(self.hyperfine(tag, "print/fair.c", 10, 1, None, &fair)?);
                let status = format!("{dffy} --name-status --color=never v6.0..v6.1");
                rows.push(self.hyperfine(tag, "print/name-status", 10, 1, None, &status)?);
                let args = [
                    "v6.0..v6.1",
                    "--paging=never",
                    "--color=always",
                    "--no-highlight",
                ];
                rows.extend(self.peak_rss(
                    tag,
                    "print/range no-highlight peak RSS",
                    set,
                    &args,
                    None,
                )?);
            }
            "pager" => {
                let pager = format!("{} --paging=never --color=always", quote(set.join("dffy")));
                rows.push(self.hyperfine(tag, "pager/range", 5, 1, Some(&self.patch), &pager)?);
                let args = ["--paging=never", "--color=always"];
                rows.extend(self.peak_rss(
                    tag,
                    "pager/range peak RSS",
                    set,
                    &args,
                    Some(&self.patch),
                )?);
            }
            _ => return Err(format!("unknown group {group}")),
        }
        Ok(rows)
    }

    /// hyperfine's median for `command`, run without a shell and with
    /// dffy's disk cache emptied before every run.
    fn hyperfine(
        &self,
        tag: &str,
        name: &str,
        runs: u32,
        warmup: u32,
        input: Option<&Path>,
        command: &str,
    ) -> Result<Row> {
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
        cmd.arg(command);
        self.run(&mut cmd, tag, name)?;
        Ok((
            name.to_owned(),
            Unit::Time,
            Timing::read(&json)?.median * 1e9,
        ))
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
        Ok(Some((name.to_owned(), Unit::Bytes, bytes)))
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
            rows.push((bench.clone(), Unit::Time, nanos(median)?));
            continue;
        }
        match first.trim_start_matches(['│', ' ']).trim_end() {
            "alloc:" => alloc = 2,
            label if label.ends_with(':') => alloc = 0,
            _ if alloc == 2 => {
                rows.push((format!("{bench} alloc count"), Unit::Count, count(mean)?));
                alloc = 1;
            }
            _ if alloc == 1 => {
                rows.push((format!("{bench} alloc bytes"), Unit::Bytes, bytes(mean)?));
                alloc = 0;
            }
            _ => {}
        }
    }
    Ok(rows)
}

/// The largest difference between the two sides of one round.
fn band(m: &Metric) -> f64 {
    m.values[0]
        .iter()
        .zip(&m.values[1])
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max)
}

fn read_noise(path: &Path) -> Option<HashMap<String, f64>> {
    let text = fs::read_to_string(path).ok()?;
    let bands = text
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split('\t');
            Some((fields.next()?.to_owned(), fields.next()?.parse().ok()?))
        });
    Some(bands.collect())
}

/// Writes the measured bands into `path`, keeping the bands of metrics this
/// run didn't measure, so a run of some groups doesn't ungate the others.
fn write_noise(path: &Path, metrics: &[Metric], rounds: usize) -> Result<()> {
    let measured = |name: &str| metrics.iter().any(|m| m.name == name);
    let mut out = format!(
        "# cargo xtask bench-noise ({rounds} rounds, {}): metric, the largest difference between A and B of one round with A and B the same set, unit\n",
        now()
    );
    if let Ok(old) = fs::read_to_string(path) {
        for line in old.lines().filter(|l| !l.starts_with('#')) {
            if !measured(line.split('\t').next().unwrap_or_default()) {
                out += line;
                out.push('\n');
            }
        }
    }
    for m in metrics.iter().filter(|m| m.name != COUNT_MISMATCH) {
        let _ = writeln!(out, "{}\t{}\t{}", m.name, band(m), m.unit.symbol());
    }
    fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
}

enum Gate<'a> {
    /// A compared with itself: the table shows the bands being measured.
    Measuring,
    /// No noise file: deltas are reported, and only `count_pass` fails.
    Uncalibrated,
    Bands(&'a HashMap<String, f64>),
}

/// The markdown table and whether every metric passed.
fn table(metrics: &[Metric], gate: &Gate) -> (String, bool) {
    let mut out =
        "| Metric | A | B | Delta | Band | Result |\n|---|---|---|---|---|---|\n".to_owned();
    let (mut failed, mut ungated) = (0, 0);
    for m in metrics {
        let a = median(&mut m.values[0].clone());
        let b = median(&mut m.values[1].clone());
        let delta = match (a, b) {
            (Some(a), Some(b)) if a > 0.0 => format!("{:+.1}%", (b - a) / a * 100.0),
            (Some(_), Some(b)) if b > 0.0 => "—".to_owned(),
            (Some(_), Some(_)) => "+0.0%".to_owned(),
            _ => "—".to_owned(),
        };
        let mut band_cell = "—".to_owned();
        let result = if m.name == COUNT_MISMATCH {
            let sides = if matches!(gate, Gate::Measuring) {
                0..2
            } else {
                1..2
            };
            if m.values[sides].iter().flatten().any(|&n| n > 0.0) {
                "FAIL"
            } else {
                "PASS"
            }
        } else {
            match (a, b, gate) {
                (_, _, Gate::Measuring) => {
                    band_cell = show(band(m), m.unit);
                    "measured"
                }
                (None, _, _) => "only in B",
                (_, None, Gate::Uncalibrated) => "missing in B",
                (_, None, Gate::Bands(_)) => "FAIL (missing in B)",
                (Some(a), Some(b), Gate::Bands(bands)) if bands.contains_key(&m.name) => {
                    let band = bands[&m.name];
                    band_cell = show(band, m.unit);
                    if b <= a + band { "PASS" } else { "FAIL" }
                }
                _ => {
                    ungated += 1;
                    "not gated"
                }
            }
        };
        if result.starts_with("FAIL") {
            failed += 1;
        }
        let cell = |v: Option<f64>| v.map_or_else(|| "—".to_owned(), |v| show(v, m.unit));
        let _ = writeln!(
            out,
            "| {} | {} | {} | {delta} | {band_cell} | {result} |",
            m.name,
            cell(a),
            cell(b)
        );
    }
    if ungated > 0 && matches!(gate, Gate::Bands(_)) {
        let _ = write!(
            out,
            "\n{ungated} metrics have no band in noise.tsv and are not gated.\n"
        );
    }
    let _ = write!(out, "\nFailed: {failed} of {} metrics.\n", metrics.len());
    (out, failed == 0)
}

/// Four significant digits in the largest unit under the value.
fn show(v: f64, unit: Unit) -> String {
    let scales: &[(f64, &str)] = match unit {
        Unit::Time => &[(1e9, "s"), (1e6, "ms"), (1e3, "µs"), (1.0, "ns")],
        Unit::Bytes => &[(1e9, "GB"), (1e6, "MB"), (1e3, "KB"), (1.0, "B")],
        Unit::Count => return format!("{v}"),
    };
    if v.abs() < 1e-9 {
        return "0".to_owned();
    }
    let (scale, suffix) = scales
        .iter()
        .find(|(s, _)| v.abs() >= *s)
        .copied()
        .unwrap_or((1.0, scales[3].1));
    let x = v / scale;
    let digits = if x.abs() >= 100.0 {
        1
    } else if x.abs() >= 10.0 {
        2
    } else {
        3
    };
    format!("{x:.digits$} {suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
