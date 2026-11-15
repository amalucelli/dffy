//! `cargo xtask compare`: dffy timed against delta, diff-so-fancy,
//! difftastic, hunk and lumen on the kernel's v6.0..v6.1, printed as the
//! tables in `docs/benchmarks.md`. The clone is never modified; the
//! working-tree viewer table needs uncommitted changes in it and is skipped
//! without them.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::harness::{self, BUSY, Memory, Run, TIMEOUT};
use crate::{Result, Sandbox, Timing, USAGE, bench_repo, median, now, output, which};

const SCENARIOS: [&str; 5] = ["range", "renames", "file", "syntax", "tui"];
const FAIR: &str = "kernel/sched/fair.c";
const TABLE: &str = "drivers/net/wireless/realtek/rtw89/rtw8852c_table.c";
const FILES: [&str; 5] = [FAIR, "kernel/exit.c", "mm/mmap.c", "mm/vmscan.c", TABLE];
/// On the first changed line of the range's first file, .clang-format.
const FIRST_CHANGE: &str = "ping_portaddr_for_each_entry_rcu";

/// Each tool from its variable or else from PATH; `None` is skipped.
struct Tools {
    dffy: Option<String>,
    delta: Option<String>,
    difft: Option<String>,
    dsf: Option<String>,
    /// hunk's native binary rather than its Node launcher, so the
    /// launcher's startup isn't counted.
    hunk: Option<String>,
    lumen: Option<String>,
}

struct Compare {
    sandbox: Sandbox,
    tools: Tools,
    hyperfine: String,
    runs: usize,
    /// For commands that take more than about ten seconds.
    slow_runs: usize,
    mem_runs: usize,
}

pub(crate) fn main(args: &[String]) -> Result<()> {
    let env = |var: &str, default: &str| std::env::var(var).unwrap_or_else(|_| default.to_owned());
    let mut scenarios = env("SCENARIOS", &SCENARIOS.join(","));
    let mut counts = [
        ("RUNS", env("RUNS", "10")),
        ("SLOW_RUNS", env("SLOW_RUNS", "3")),
        ("MEM_RUNS", env("MEM_RUNS", "3")),
    ];
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let slot = match arg.as_str() {
            "--scenarios" => &mut scenarios,
            "--runs" => &mut counts[0].1,
            "--slow-runs" => &mut counts[1].1,
            "--mem-runs" => &mut counts[2].1,
            _ => return Err(USAGE.to_owned()),
        };
        slot.clone_from(args.next().ok_or(USAGE)?);
    }
    let [runs, slow_runs, mem_runs] = counts.map(|(var, n)| {
        n.parse()
            .ok()
            .filter(|&n| n > 0)
            .ok_or_else(|| format!("{var} must be a positive number"))
    });
    let scenarios: Vec<&str> = scenarios
        .split([',', ' '])
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(bad) = scenarios.iter().find(|s| !SCENARIOS.contains(s)) {
        return Err(format!(
            "unknown scenario {bad}; scenarios are {}",
            SCENARIOS.join(", ")
        ));
    }
    let tool =
        |var: &str, default: &str| which(&env(var, default)).map(|p| p.display().to_string());
    let compare = Compare {
        sandbox: Sandbox::new(bench_repo()?, true)?,
        tools: Tools {
            dffy: tool("DFFY", "dffy"),
            delta: tool("DELTA", "delta"),
            difft: tool("DIFFT", "difft"),
            dsf: tool("DIFF_SO_FANCY", "diff-so-fancy"),
            hunk: tool("HUNK", "hunk"),
            lumen: tool("LUMEN", "lumen"),
        },
        hyperfine: tool("HYPERFINE", "hyperfine").ok_or("hyperfine not found")?,
        runs: runs?,
        slow_runs: slow_runs?,
        mem_runs: mem_runs?,
    };
    compare.header()?;
    for (path, rev, side) in FILES
        .iter()
        .flat_map(|f| [(f, "v6.0", "old"), (f, "v6.1", "new")])
    {
        let to = compare.sandbox.path(side).join(path);
        fs::create_dir_all(to.parent().unwrap_or(&to))
            .map_err(|e| format!("{}: {e}", to.display()))?;
        let file = fs::File::create(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        output(
            compare
                .sandbox
                .command("git")
                .args(["show", &format!("{rev}:{path}")])
                .stdout(file),
        )?;
    }
    for scenario in scenarios {
        match scenario {
            "range" => compare.range()?,
            "renames" => compare.renames()?,
            "file" => compare.file()?,
            "syntax" => compare.syntax()?,
            _ => compare.tui()?,
        }
    }
    Ok(())
}

/// Waits until the one-minute load average is below 1, so a measurement
/// doesn't start while the previous one's processes, or anything else,
/// still load the machine. Gives up after five minutes on a machine that
/// stays busy. Only where `/proc/loadavg` exists.
fn idle() {
    let load = || {
        fs::read_to_string("/proc/loadavg")
            .ok()?
            .split_whitespace()
            .next()?
            .parse::<f64>()
            .ok()
    };
    let start = Instant::now();
    while load().is_some_and(|l| l >= 1.0) {
        if start.elapsed() >= Duration::from_secs(300) {
            eprintln!("load average still at 1 or more after 5 minutes; measuring anyway");
            return;
        }
        thread::sleep(Duration::from_secs(10));
    }
}

/// `12.3ms (11.0–14.2)`: median, min and max of milliseconds.
fn summary(values: &[f64]) -> String {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(0.0, f64::max);
    let med = median(&mut values.to_vec()).unwrap_or_default();
    format!("{med:.1}ms ({min:.1}–{max:.1})")
}

fn stats(t: &Timing) -> String {
    let f = |v: f64| {
        if t.max < 1.0 {
            format!("{:.1}ms", v * 1000.0)
        } else {
            format!("{v:.2}s")
        }
    };
    format!(
        "{} ± {} ({}–{})",
        f(t.median),
        f(t.stddev),
        f(t.min),
        f(t.max)
    )
}

fn mb(kb: f64) -> String {
    if kb < 1_048_576.0 {
        format!("{:.1}MB", kb / 1024.0)
    } else {
        format!("{:.2}GB", kb / 1_048_576.0)
    }
}

/// Three table cells: the tool's own memory, the files it has mapped, and
/// git's own / mapped when git runs beside it. The tool is the program
/// other than git with the most memory of its own.
fn memory_cells(memory: &Memory) -> String {
    let runs = match memory {
        Memory::Peak(kb) => {
            return format!(
                "{} | — | —",
                median(&mut kb.clone()).map_or_else(|| "—".to_owned(), mb)
            );
        }
        Memory::Programs(runs) => runs,
    };
    let mut medians: HashMap<&str, (f64, f64)> = HashMap::new();
    for name in runs.iter().flat_map(HashMap::keys) {
        let seen = runs.iter().filter_map(|peaks| peaks.get(name));
        let (mut anon, mut mapped): (Vec<f64>, Vec<f64>) = seen.copied().unzip();
        medians.insert(
            name,
            (
                median(&mut anon).unwrap_or_default(),
                median(&mut mapped).unwrap_or_default(),
            ),
        );
    }
    let tool = medians
        .iter()
        .filter(|(name, _)| **name != "git")
        .max_by(|a, b| a.1.0.total_cmp(&b.1.0))
        .map(|(name, _)| *name)
        .or_else(|| medians.contains_key("git").then_some("git"));
    let Some(tool) = tool else {
        return "— | — | —".to_owned();
    };
    let other = match medians.get("git") {
        Some(&(anon, mapped)) if tool != "git" => format!("{} / {}", mb(anon), mb(mapped)),
        _ => "—".to_owned(),
    };
    let (anon, mapped) = medians[tool];
    format!("{} | {} | {other}", mb(anon), mb(mapped))
}

fn first_line(out: &str) -> &str {
    out.lines().next().unwrap_or_default()
}

impl Compare {
    fn tools(&self) -> impl Iterator<Item = &String> {
        let t = &self.tools;
        [&t.dffy, &t.delta, &t.difft, &t.dsf, &t.hunk, &t.lumen]
            .into_iter()
            .flatten()
    }

    /// A command as the tables show it: tools by name, files relative to
    /// the sandbox, and pipes escaped for markdown.
    fn show(&self, command: &str) -> String {
        let mut s = command.to_owned();
        for tool in self.tools() {
            let name = Path::new(tool)
                .file_name()
                .map_or_else(|| tool.clone(), |n| n.to_string_lossy().into_owned());
            s = s.replace(tool.as_str(), &name);
        }
        s.replace(&format!("{}/", self.sandbox.root.display()), "")
            .replace('|', "\\|")
    }

    /// The first line of a command's stdout, empty when it fails.
    fn version(&self, program: &str, args: &[&str]) -> String {
        output(self.sandbox.command(program).args(args))
            .map(|(out, _)| first_line(&out).to_owned())
            .unwrap_or_default()
    }

    /// A command's stdout and stderr together, whatever its exit status.
    fn text(&self, command: &str) -> String {
        self.sandbox
            .command("sh")
            .args(["-c", &format!("{command} 2>&1")])
            .stdin(Stdio::null())
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default()
    }

    fn header(&self) -> Result<()> {
        let sandbox = &self.sandbox;
        let (describe, _) = output(
            sandbox
                .command("git")
                .args(["describe", "--always", "v6.1"]),
        )?;
        let (names, _) = output(sandbox.command("git").args([
            "diff",
            "--no-ext-diff",
            "--name-only",
            "v6.0",
            "v6.1",
        ]))?;
        println!("# dffy comparison benchmark\n");
        println!("- Date: {}", now());
        println!("- Machine: {}", machine());
        if let Ok(load) = fs::read_to_string("/proc/loadavg") {
            println!(
                "- Load average at start: {}",
                load.split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        println!(
            "- Repository: {} ({} files changed in v6.0..v6.1)",
            describe.trim(),
            names.lines().count()
        );
        println!(
            "- Runs: {} ({} for commands over ~10s, {} sampling memory), warm page cache, dffy's disk cache emptied before each run",
            self.runs, self.slow_runs, self.mem_runs
        );
        if !cfg!(target_os = "linux") {
            println!(
                "- Memory: without /proc, own memory is the peak RSS of the whole command from {} {}, mapped files and git's share aren't measured, and the viewer tables have none",
                crate::TIME,
                crate::time_flag()
            );
        }
        let word = |s: String, n: usize| s.split(' ').nth(n).unwrap_or_default().to_owned();
        let dffy = self
            .tools
            .dffy
            .as_deref()
            .map(|d| word(self.version(d, &["--version"]), 1));
        println!(
            "- Versions: dffy {}, {}, {}, hyperfine {}",
            dffy.unwrap_or_default(),
            self.version("git", &["--version"]),
            self.version("diff", &["--version"]),
            word(self.version(&self.hyperfine, &["--version"]), 1),
        );
        for tool in [&self.tools.delta, &self.tools.difft].into_iter().flatten() {
            println!("  {}", self.version(tool, &["--version"]));
        }
        // diff-so-fancy prints its version only when stdin is a terminal.
        if let Some(dsf) = &self.tools.dsf {
            let script = fs::canonicalize(dsf)
                .and_then(fs::read_to_string)
                .unwrap_or_default();
            let version = script
                .lines()
                .find_map(|l| l.strip_prefix("my $VERSION = \"")?.strip_suffix("\";"));
            println!("  diff-so-fancy {}", version.unwrap_or_default());
        }
        if let Some(hunk) = &self.tools.hunk {
            println!("  hunk {}", self.version(hunk, &["--version"]));
        }
        if let Some(lumen) = &self.tools.lumen {
            println!("  {}", self.version(lumen, &["--version"]));
        }
        Ok(())
    }

    /// hyperfine's wall time for a shell command.
    fn hyperfine(&self, command: &str, warmup: usize, runs: usize, fresh: bool) -> Result<Timing> {
        let json = self.sandbox.path("h.json");
        let mut cmd = self.sandbox.command(&self.hyperfine);
        cmd.args([
            "--shell=sh",
            "--ignore-failure",
            "--style",
            "none",
            "--export-json",
        ])
        .arg(&json)
        .args(["--warmup", &warmup.to_string(), "--runs", &runs.to_string()])
        .stdout(Stdio::null());
        if fresh {
            cmd.args([
                "--prepare",
                &format!("rm -rf {}", crate::quote(self.sandbox.dffy_cache())),
            ]);
        }
        output(cmd.arg(command))?;
        Timing::read(&json)
    }

    fn table_header() {
        println!();
        println!(
            "| Tool | Command | First byte, median (min–max) | Total, median ± σ (min–max) | Own memory | Mapped files | git beside it, own / mapped |"
        );
        println!("|---|---|---|---|---|---|---|");
    }

    /// One table row for `cmd` reading `input`'s output, or `cmd` alone
    /// when `input` is empty: total time from hyperfine, first byte and
    /// memory from the harness.
    fn row(&self, name: &str, runs: usize, input: &str, cmd: &str) -> Result<()> {
        let full = if input.is_empty() {
            cmd.to_owned()
        } else {
            format!("{input} | {cmd}")
        };
        idle();
        let time = stats(&self.hyperfine(&full, usize::from(runs == self.runs), runs, true)?);
        idle();
        let first = summary(&harness::first_byte(&self.sandbox, runs, &full)?);
        idle();
        let memory = memory_cells(&harness::memory(
            &self.sandbox,
            self.mem_runs.min(runs),
            &full,
        )?);
        println!(
            "| {name} | `{}` | {first} | {time} | {memory} |",
            self.show(&full)
        );
        Ok(())
    }

    fn range(&self) -> Result<()> {
        let (t, runs, slow) = (&self.tools, self.runs, self.slow_runs);
        println!("\n## The whole v6.0..v6.1 range, printed");
        Self::table_header();
        let git_in = "git diff --no-ext-diff --color=always v6.0 v6.1";
        self.row("git diff (baseline)", runs, "", git_in)?;
        if let Some(dffy) = &t.dffy {
            self.row(
                "dffy, as git's pager",
                runs,
                git_in,
                &format!("{dffy} --paging=never --color=always"),
            )?;
            let range = format!("{dffy} v6.0..v6.1 --paging=never --color=always");
            self.row(
                "dffy, on the repository, no highlighting",
                runs,
                "",
                &format!("{range} --no-highlight"),
            )?;
            self.row("dffy, on the repository, highlighted", slow, "", &range)?;
        }
        if let Some(delta) = &t.delta {
            self.row(
                "delta, no highlighting",
                slow,
                git_in,
                &format!("{delta} --paging=never --syntax-theme=none"),
            )?;
            self.row(
                "delta, highlighted",
                slow,
                git_in,
                &format!("{delta} --paging=never"),
            )?;
        }
        if let Some(dsf) = &t.dsf {
            self.row("diff-so-fancy", slow, git_in, dsf)?;
        }
        if let Some(difft) = &t.difft {
            let runs = std::env::var("DIFFT_RANGE_RUNS")
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
            let cmd = format!("DFT_COLOR=always GIT_EXTERNAL_DIFF={difft} git diff v6.0 v6.1");
            self.row("difftastic, as git's external diff", runs, "", &cmd)?;
        }
        Ok(())
    }

    fn renames(&self) -> Result<()> {
        println!("\n## The range's file list, with rename detection");
        Self::table_header();
        let git = "git diff --no-ext-diff --name-status v6.0 v6.1";
        self.row("git diff --name-status", self.runs, "", git)?;
        let Some(dffy) = &self.tools.dffy else {
            return Ok(());
        };
        let cmd = format!("{dffy} --name-status --color=never v6.0..v6.1");
        self.row("dffy --name-status", self.runs, "", &cmd)?;
        let list = |command: &str| -> Result<Vec<u8>> {
            let out = self.sandbox.command("sh").args(["-c", command]).output();
            out.map(|o| o.stdout).map_err(|e| format!("{command}: {e}"))
        };
        let (git, dffy) = (list(git)?, list(&cmd)?);
        if git == dffy {
            let text = String::from_utf8_lossy(&git);
            let renames = text.lines().filter(|l| l.starts_with('R')).count();
            println!(
                "\nBoth lists are identical: {} entries, {renames} of them renames.",
                text.lines().count()
            );
        } else {
            println!("\nThe lists differ.");
        }
        Ok(())
    }

    fn file(&self) -> Result<()> {
        let (t, runs) = (&self.tools, self.runs);
        let old = self.sandbox.path("old").join(FAIR).display().to_string();
        let new = self.sandbox.path("new").join(FAIR).display().to_string();
        println!("\n## One large file: {FAIR}, v6.0 to v6.1");
        Self::table_header();
        self.row("GNU diff", runs, "", &format!("diff -u {old} {new}"))?;
        let git = format!("git diff --no-index --color=always {old} {new}");
        self.row("git diff --no-index", runs, "", &git)?;
        if let Some(dffy) = &t.dffy {
            self.row(
                "dffy",
                runs,
                "",
                &format!("{dffy} --paging=never --color=always {old} {new}"),
            )?;
        }
        if let Some(delta) = &t.delta {
            self.row(
                "delta",
                runs,
                "",
                &format!("{delta} --paging=never {old} {new}"),
            )?;
        }
        if let Some(dsf) = &t.dsf {
            self.row("diff-so-fancy", runs, &git, dsf)?;
        }
        if let Some(dffy) = &t.dffy {
            let cmd = format!("{dffy} --syntax --paging=never --color=always {old} {new}");
            self.row("dffy --syntax", runs, "", &cmd)?;
        }
        if let Some(difft) = &t.difft {
            self.row(
                "difftastic",
                runs,
                "",
                &format!("DFT_COLOR=always {difft} {old} {new}"),
            )?;
        }
        Ok(())
    }

    fn syntax(&self) -> Result<()> {
        println!("\n## Syntax-aware diffs of five large files\n");
        println!(
            "| File (old → new) | Tool | Time, median ± σ (min–max) | Own memory | Mapped files | What it showed |"
        );
        println!("|---|---|---|---|---|---|");
        for f in FILES {
            let (old, new) = (
                self.sandbox.path("old").join(f),
                self.sandbox.path("new").join(f),
            );
            let kb = |p: &Path| p.metadata().map_or(0, |m| m.len() / 1024);
            let size = format!("{}KB → {}KB", kb(&old), kb(&new));
            let (old, new) = (old.display(), new.display());
            // Memory without git's column: no git runs beside these.
            let memory = |cmd: &str| -> Result<String> {
                let cells = memory_cells(&harness::memory(&self.sandbox, self.mem_runs, cmd)?);
                Ok(cells
                    .rsplit_once(" | ")
                    .map_or_else(|| cells.clone(), |(cells, _)| cells.to_owned()))
            };
            if let Some(dffy) = &self.tools.dffy {
                let cmd = format!("{dffy} --syntax --paging=never --color=never {old} {new}");
                idle();
                let time = stats(&self.hyperfine(&cmd, 1, self.runs, true)?);
                let memory = memory(&cmd)?;
                self.sandbox.empty_dffy_cache()?;
                // dffy writes a note into the diff when a region or the whole
                // file kept the line diff ("no syntax diff: ...", "syntax diff: ...").
                let text = self.text(&cmd);
                let note = text
                    .lines()
                    .find(|l| l.contains("syntax diff"))
                    .unwrap_or("syntax diff, nothing fell back");
                println!("| {f} ({size}) | dffy --syntax | {time} | {memory} | {note} |");
            }
            if let Some(difft) = &self.tools.difft {
                let cmd = format!("DFT_COLOR=never {difft} {old} {new}");
                idle();
                let time = stats(&self.hyperfine(&cmd, 1, self.runs, false)?);
                let memory = memory(&cmd)?;
                // difftastic names a fallback in its file header, after the language.
                let text = self.text(&cmd);
                let header = first_line(&text);
                let note = header.split_once(" --- ").map_or(header, |(_, note)| note);
                println!("| {f} ({size}) | difftastic | {time} | {memory} | {note} |");
            }
        }
        Ok(())
    }

    /// One viewer row, run in the pty until `matched` is on screen. A
    /// viewer that doesn't show it in time gets ">60s", and one that shows
    /// `refusal` (if not empty) instead gets "n/a".
    fn pty(
        &self,
        name: &str,
        runs: usize,
        matched: &str,
        refusal: &str,
        argv: &[&str],
    ) -> Result<()> {
        idle();
        let (results, peaks) = harness::pty(&self.sandbox, runs, argv, matched, refusal)?;
        let times = match results.last() {
            Some(Run::Timeout) => format!(">{0}s | >{0}s | >{0}s", TIMEOUT.as_secs()),
            Some(Run::Refused) => format!("n/a | n/a (shows \"{refusal}\") | n/a"),
            _ => {
                let shown: Vec<_> = results
                    .iter()
                    .filter_map(|r| {
                        if let Run::Shown(byte, frame, settled) = r {
                            Some((*byte, *frame, *settled))
                        } else {
                            None
                        }
                    })
                    .collect();
                let column = |f: fn(&(f64, f64, Option<f64>)) -> f64| {
                    summary(&shown.iter().map(f).collect::<Vec<_>>())
                };
                let settled: Option<Vec<f64>> = shown.iter().map(|r| r.2).collect();
                let settled = settled.map_or_else(
                    || format!("still drawing {}s later", BUSY.as_secs()),
                    |s| summary(&s),
                );
                format!("{} | {} | {settled}", column(|r| r.0), column(|r| r.1))
            }
        };
        // A pipeline runs under sh -c; the table shows the pipeline.
        let cmd = argv.join(" ");
        let cmd = cmd.strip_prefix("sh -c ").unwrap_or(&cmd);
        println!(
            "| {name} | `{}` | {times} | {} |",
            self.show(cmd),
            memory_cells(&Memory::Programs(peaks))
        );
        // hunk starts a session daemon on first use, outside the viewer's session.
        if let Some(hunk) = &self.tools.hunk {
            Command::new("pkill")
                .args(["-f", &format!("{hunk} daemon serve")])
                .status()
                .ok();
        }
        Ok(())
    }

    fn viewers(title: &str, note: &str) {
        println!("\n## Interactive viewers: {title}\n\n{note}\n");
        println!(
            "| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |"
        );
        println!("|---|---|---|---|---|---|---|---|");
    }

    fn tui(&self) -> Result<()> {
        let (t, runs, slow) = (&self.tools, self.runs, self.slow_runs);
        let first = FIRST_CHANGE;
        Self::viewers(
            "v6.0..v6.1, 200×60 pty",
            "First frame: the first changed line of the first file (in .clang-format) is on screen. Settled: the last byte before a second with no output.",
        );
        self.pty(
            "harness floor",
            runs,
            first,
            "",
            &["sh", "-c", &format!("echo {first}; sleep 1.5")],
        )?;
        if let Some(dffy) = &t.dffy {
            self.pty("dffy", runs, first, "", &[dffy, "v6.0..v6.1"])?;
        }
        if let Some(lumen) = &t.lumen {
            self.pty("lumen", slow, first, "", &[lumen, "diff", "v6.0..v6.1"])?;
        }
        if let Some(hunk) = &t.hunk {
            self.pty("hunk", slow, first, "", &[hunk, "diff", "v6.0", "v6.1"])?;
        }

        // The first added line of the first changed file, as each viewer opens there.
        let (diff, _) = output(self.sandbox.command("git").args(["diff", "--no-ext-diff"]))?;
        let edit: Option<String> = diff
            .lines()
            .find_map(|l| {
                l.strip_prefix('+')
                    .filter(|rest| !rest.is_empty() && !rest.starts_with('+'))
            })
            .map(|l| l.chars().take(40).collect());
        if let Some(edit) = edit {
            let (names, _) =
                output(
                    self.sandbox
                        .command("git")
                        .args(["diff", "--no-ext-diff", "--name-only"]),
                )?;
            Self::viewers(
                "the working tree, 200×60 pty",
                &format!(
                    "Uncommitted changes to {} files. First frame: the first added line of the first file is on screen.",
                    names.lines().count()
                ),
            );
            if let Some(dffy) = &t.dffy {
                self.pty("dffy", runs, &edit, "", &[dffy])?;
            }
            if let Some(lumen) = &t.lumen {
                self.pty("lumen", runs, &edit, "", &[lumen, "diff"])?;
            }
            if let Some(hunk) = &t.hunk {
                self.pty("hunk", runs, &edit, "", &[hunk, "diff"])?;
            }
        } else {
            println!("\nWorking tree skipped: the clone has no uncommitted changes.");
        }
        self.large_files()?;
        self.pager()
    }

    fn large_files(&self) -> Result<()> {
        let (t, runs) = (&self.tools, self.runs);
        // hunk 0.25.0 declines rtw8852c_table.c (1.1MB) with this notice.
        let large = "File too large to render automatically.";
        for (path, matched) in [
            (FAIR, "memory-tiers.h"),
            (TABLE, "rtw89_8852c_phy_radioa_regs"),
        ] {
            let old = self.sandbox.path("old").join(path).display().to_string();
            let new = self.sandbox.path("new").join(path).display().to_string();
            Self::viewers(
                &format!("one large file, {path}, 200×60 pty"),
                "First frame: the file's first changed line is on screen. Each viewer reads it from the repository with a path filter, and from two extracted files where it can.",
            );
            if let Some(dffy) = &t.dffy {
                self.pty("dffy", runs, matched, "", &[dffy, "v6.0..v6.1", "--", path])?;
                self.pty("dffy, two files", runs, matched, "", &[dffy, &old, &new])?;
            }
            if let Some(lumen) = &t.lumen {
                self.pty(
                    "lumen",
                    runs,
                    matched,
                    "",
                    &[lumen, "diff", "v6.0..v6.1", "--file", path],
                )?;
                println!("| lumen, two files | | n/a | n/a (no two-file mode) | n/a | | | |");
            }
            if let Some(hunk) = &t.hunk {
                self.pty(
                    "hunk",
                    runs,
                    matched,
                    large,
                    &[hunk, "diff", "v6.0", "v6.1", "--", path],
                )?;
                self.pty(
                    "hunk, two files",
                    runs,
                    matched,
                    large,
                    &[hunk, "diff", "--files", &old, &new],
                )?;
            }
        }
        Ok(())
    }

    fn pager(&self) -> Result<()> {
        let (t, runs, slow) = (&self.tools, self.runs, self.slow_runs);
        let first = FIRST_CHANGE;
        let git_in = "git diff --no-ext-diff v6.0 v6.1";
        Self::viewers(
            "as a pager, v6.0..v6.1, 200×60 pty",
            &format!(
                "The viewer reads the range's patch from `{git_in}` on stdin, with the pty as its terminal. First frame: the first changed line of .clang-format is on screen."
            ),
        );
        if let Some(dffy) = &t.dffy {
            self.pty(
                "dffy",
                runs,
                first,
                "",
                &["sh", "-c", &format!("{git_in} | {dffy}")],
            )?;
        }
        if let Some(hunk) = &t.hunk {
            self.pty(
                "hunk pager",
                slow,
                first,
                "",
                &["sh", "-c", &format!("{git_in} | {hunk} pager")],
            )?;
        }
        if t.lumen.is_some() {
            println!(
                "| lumen | | n/a | n/a (not a pager: reads revisions, not a patch) | n/a | | | |"
            );
        }
        Ok(())
    }
}

fn machine() -> String {
    let cores = thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let uname = output(Command::new("uname").arg("-sr"))
        .map(|(s, _)| s.trim().to_owned())
        .unwrap_or_default();
    let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") else {
        return format!("{cores} cores, {uname}");
    };
    let model = cpuinfo
        .lines()
        .find_map(|l| Some(l.strip_prefix("model name")?.split_once(':')?.1.trim()));
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb: f64 = meminfo
        .lines()
        .find_map(|l| {
            l.strip_prefix("MemTotal:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or_default();
    let release = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let os = release
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME=\"")?.strip_suffix('"'))
        .unwrap_or_default();
    format!(
        "{}, {cores} cores, {:.0}GB RAM, {os}, {uname}",
        model.unwrap_or_default(),
        kb / 1_048_576.0
    )
}
