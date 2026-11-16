//! `cargo xtask`: dffy's benchmark tooling, on macOS and Linux.
//!
//! - `perf` is the performance gate: the checkout compared with a build of
//!   another commit on the same machine, failing on a slowdown consistent
//!   across rounds, more instructions or an allocation increase. `bench-*`
//!   are its steps, for running by hand.
//! - `compare` times dffy against other diff tools on the kernel range and
//!   prints the tables in `docs/benchmarks.md`.
//! - `report` prints a run's tables from its `results.json`.

mod ab;
mod compare;
mod harness;
mod perf;
mod results;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
usage: cargo xtask perf [--quick | --full] [--rev REV] [--rounds N] [--only GROUP,...]
       cargo xtask compare [--scenarios S,...] [--runs N] [--slow-runs N] [--mem-runs N]
       cargo xtask report <run-dir>
advanced:
       cargo xtask bench-baseline <dir>
       cargo xtask bench-ab <base-dir> [--quick] [--rounds N] [--only GROUP,...] [--instructions METRIC,...]
       cargo xtask bench-noise <base-dir> [--rounds N] [--only GROUP,...]
       cargo xtask bench-layouts <base-dir> [--layouts N] [--rounds N] [--only GROUP,...]

perf measures the checkout against a build of --rev (default HEAD~1), every
group unless --only narrows it; --quick (Linux) only instruction and
allocation counts, of every group; --full with fresh layout bands.
GROUP is one of frame, engine, pty, count, paint, print, pager.
S is one of range, renames, file, syntax, tui; tui is every viewer table.
compare's defaults come from SCENARIOS, RUNS (10), SLOW_RUNS (3) and
MEM_RUNS (3), and its tools from DFFY, DELTA, DIFFT, DIFF_SO_FANCY, HUNK,
LUMEN and HYPERFINE or else PATH; a missing tool is skipped.
DFFY_BENCH_REPO names the kernel clone with tags v6.0 and v6.1.";

/// Where cargo builds the benchmark executables.
const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

type Result<T, E = String> = std::result::Result<T, E>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((cmd, rest)) if cmd == "perf" => perf::main(rest),
        Some((cmd, rest)) if cmd.starts_with("bench-") => ab::main(cmd, rest),
        Some((cmd, rest)) if cmd == "compare" => compare::main(rest).map(|()| true),
        Some((cmd, rest)) if cmd == "report" => results::main(rest).map(|()| true),
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::from(2)
        }
    }
}

/// fish passes `~` through task and cargo unexpanded.
fn expand_tilde(path: &str) -> PathBuf {
    let home = || PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if path == "~" {
        home()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(path)
    }
}

fn bench_repo() -> Result<PathBuf> {
    let repo = std::env::var("DFFY_BENCH_REPO")
        .map_err(|_| "point DFFY_BENCH_REPO at a Linux clone with tags v6.0 and v6.1")?;
    fs::canonicalize(expand_tilde(&repo)).map_err(|e| format!("DFFY_BENCH_REPO={repo}: {e}"))
}

/// `name` as `command -v` finds it: a path, made absolute since commands
/// run in the kernel clone, or the first executable of that name on PATH.
fn which(name: &str) -> Option<PathBuf> {
    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        let path = std::path::absolute(expand_tilde(name)).ok()?;
        return executable(&path).then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| executable(p))
}

/// Single-quoted for sh, and for the word splitting hyperfine does itself
/// without a shell.
fn quote(s: impl AsRef<OsStr>) -> String {
    format!("'{}'", s.as_ref().to_string_lossy().replace('\'', r"'\''"))
}

fn describe(cmd: &Command) -> String {
    let mut s = cmd.get_program().display().to_string();
    for arg in cmd.get_args() {
        s.push(' ');
        s.push_str(&arg.to_string_lossy());
    }
    s
}

/// Runs `cmd` to completion and returns its stdout and stderr, or an error
/// with the stderr when it fails.
fn output(cmd: &mut Command) -> Result<(String, String)> {
    let out = cmd
        .output()
        .map_err(|e| format!("{}: {e}", describe(cmd)))?;
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        return Err(format!(
            "{}: {}\n{}",
            describe(cmd),
            out.status,
            stderr.trim_end()
        ));
    }
    Ok((String::from_utf8_lossy(&out.stdout).into_owned(), stderr))
}

fn date(args: &[&str]) -> String {
    output(Command::new("date").args(args))
        .map_or_else(|_| "?".to_owned(), |(s, _)| s.trim().to_owned())
}

/// The UTC date and time the reports carry.
fn now() -> String {
    date(&["-u", "+%Y-%m-%d %H:%M UTC"])
}

fn median(values: &mut [f64]) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    match values.len() {
        0 => None,
        n if n % 2 == 1 => Some(values[mid]),
        _ => Some(f64::midpoint(values[mid - 1], values[mid])),
    }
}

/// Wall-clock seconds from hyperfine's JSON export.
struct Timing {
    median: f64,
    stddev: f64,
    min: f64,
    max: f64,
    /// Every run's.
    times: Vec<f64>,
}

impl Timing {
    fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let result = &json["results"][0];
        // hyperfine 2 nests the statistics under summary; 1.x has them on
        // the result.
        let t = match &result["summary"]["time_wall_clock"] {
            serde_json::Value::Null => result,
            t => t,
        };
        let field = |name: &str| {
            t[name]
                .as_f64()
                .ok_or_else(|| format!("{}: no wall-clock {name}", path.display()))
        };
        Ok(Self {
            median: field("median")?,
            stddev: t["stddev"].as_f64().unwrap_or(0.0),
            min: field("min")?,
            max: field("max")?,
            // hyperfine 2 keeps each run under measurements, 1.x in times.
            times: match result["measurements"].as_array() {
                Some(runs) => runs
                    .iter()
                    .filter_map(|r| r["time_wall_clock"]["value"].as_f64())
                    .collect(),
                None => result["times"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_f64)
                    .collect(),
            },
        })
    }
}

/// `run`'s result and the user-space instructions retired while it ran by
/// this thread and every thread and process it started, those that exited
/// included. None off Linux, or when the kernel offers no hardware counter.
#[cfg(target_os = "linux")]
fn instructions<T>(run: impl FnOnce() -> Result<T>) -> Result<(T, Option<f64>)> {
    use perf_event::{Builder, events::Hardware};
    // pid 0 is the calling thread; with inherit, the processes it spawns
    // add their counts to this counter when they exit.
    let Ok(mut counter) = Builder::new(Hardware::INSTRUCTIONS).inherit(true).build() else {
        return Ok((run()?, None));
    };
    counter
        .enable()
        .map_err(|e| format!("instruction counter: {e}"))?;
    let out = run()?;
    let n = counter
        .read()
        .map_err(|e| format!("instruction counter: {e}"))?;
    #[expect(
        clippy::cast_precision_loss,
        reason = "counts stay far below 2^53, where f64 is exact"
    )]
    Ok((out, Some(n as f64)))
}

#[cfg(not(target_os = "linux"))]
fn instructions<T>(run: impl FnOnce() -> Result<T>) -> Result<(T, Option<f64>)> {
    Ok((run()?, None))
}

const TIME: &str = "/usr/bin/time";

/// The flag that makes `/usr/bin/time` report peak RSS.
fn time_flag() -> &'static str {
    if cfg!(target_os = "macos") {
        "-l"
    } else {
        "-v"
    }
}

/// Peak RSS in bytes from `/usr/bin/time`'s report: macOS prints bytes,
/// GNU time kilobytes.
fn peak_rss(stderr: &str) -> Option<f64> {
    stderr.lines().find_map(|line| {
        let line = line.trim();
        if let Some(kb) = line.strip_prefix("Maximum resident set size (kbytes):") {
            kb.trim().parse::<f64>().ok().map(|kb| kb * 1024.0)
        } else if line.ends_with("maximum resident set size") {
            line.split_whitespace().next()?.parse().ok()
        } else {
            None
        }
    })
}

/// An empty home, config, state, cache, data and temporary directory, and
/// git without global or system config. Commands get only these variables,
/// `TERM`, `DFFY_BENCH_REPO` and `PATH`: divan's allocation counts change
/// with the environment. With `terminal`, they also get a UTF-8 locale and
/// true color, as in a user's terminal, so third-party tools draw as they
/// would there. Removed on drop.
struct Sandbox {
    root: PathBuf,
    repo: PathBuf,
    terminal: bool,
}

const SANDBOX_DIRS: [(&str, &str); 6] = [
    ("HOME", "home"),
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_STATE_HOME", "state"),
    ("XDG_CACHE_HOME", "cache"),
    ("XDG_DATA_HOME", "data"),
    ("TMPDIR", "tmp"),
];

impl Sandbox {
    fn new(repo: PathBuf, terminal: bool) -> Result<Self> {
        // Fixed width, so the environment's paths have the same length from
        // one invocation to the next.
        let root = std::env::temp_dir().join(format!("dffy-xtask-{:010}", std::process::id()));
        fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
        let sandbox = Self {
            root,
            repo,
            terminal,
        };
        let gitconfig = sandbox.path("gitconfig");
        fs::write(&gitconfig, "").map_err(|e| format!("{}: {e}", gitconfig.display()))?;
        sandbox.reset()?;
        Ok(sandbox)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Empties every directory in the environment.
    fn reset(&self) -> Result<()> {
        for (_, dir) in SANDBOX_DIRS {
            let path = self.path(dir);
            if path.exists() {
                fs::remove_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            }
            fs::create_dir(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(())
    }

    /// dffy's disk cache.
    fn dffy_cache(&self) -> PathBuf {
        self.path("cache/dffy")
    }

    /// Before every run, so no run reuses another's rename or syntax
    /// results.
    fn empty_dffy_cache(&self) -> Result<()> {
        let cache = self.dffy_cache();
        match fs::remove_dir_all(&cache) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("{}: {e}", cache.display()))
            }
            _ => Ok(()),
        }
    }

    fn vars(&self) -> Vec<(&'static str, OsString)> {
        let mut vars: Vec<_> = SANDBOX_DIRS
            .iter()
            .map(|&(var, dir)| (var, self.path(dir).into_os_string()))
            .collect();
        vars.extend([
            ("GIT_CONFIG_GLOBAL", self.path("gitconfig").into_os_string()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("TERM", "xterm-256color".into()),
            ("DFFY_BENCH_REPO", self.repo.clone().into_os_string()),
        ]);
        if self.terminal {
            vars.extend([
                ("LANG", "C.UTF-8".into()),
                ("COLORTERM", "truecolor".into()),
            ]);
        }
        if let Some(path) = std::env::var_os("PATH") {
            vars.push(("PATH", path));
        }
        vars
    }

    /// `program` run in the kernel clone with only the sandbox's environment.
    fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear().envs(self.vars()).current_dir(&self.repo);
        cmd
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).ok();
    }
}
