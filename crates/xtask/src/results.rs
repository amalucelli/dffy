//! A run's results, the same for every command that measures: a directory
//! under `target/perf/runs` with `results.json`, `summary.md` and each
//! program's raw output. `cargo xtask report <run-dir>` prints a run's
//! tables as markdown from its `results.json`.
//!
//! `results.json` holds a `header` (what was measured, on what), `metrics`
//! (per metric its unit, its values as named series and its verdict) and
//! `sections` (the tables of a `compare` run, cell by cell).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::{ROOT, Result, date, median, now};

/// What a run measured, and on what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Header {
    /// `perf` or `compare`.
    pub(crate) kind: String,
    /// For `perf`: `quick`, `normal`, `full` or `noise`; empty for `compare`.
    pub(crate) tier: String,
    /// The checkout's commit, and whether it had uncommitted changes.
    pub(crate) candidate: String,
    pub(crate) dirty: bool,
    /// The baseline's commit; empty without one.
    pub(crate) baseline: String,
    pub(crate) date: String,
    pub(crate) cpu: String,
    pub(crate) cores: usize,
    pub(crate) os: String,
    pub(crate) rustc: String,
    pub(crate) groups: Vec<String>,
}

/// One metric: per series (`A` and `B` per round for an A/B run, `runs`
/// for a comparison) its values, and its verdict, empty without one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    /// `ns`, `B`, `count`, `instructions`, `ms`, `s` or `KB`.
    pub(crate) unit: String,
    pub(crate) series: Vec<(String, Vec<f64>)>,
    pub(crate) result: String,
}

/// A markdown table as `report` prints it: a `## title`, a note under it,
/// the rows, and text after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Section {
    pub(crate) title: String,
    pub(crate) note: String,
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<String>>,
    pub(crate) after: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Results {
    pub(crate) header: Header,
    pub(crate) metrics: Vec<Entry>,
    pub(crate) sections: Vec<Section>,
}

/// `git args` in the checkout, its trimmed stdout, empty when it fails.
fn git(args: &[&str]) -> String {
    crate::output(Command::new("git").args(args).current_dir(ROOT))
        .map(|(out, _)| out.trim().to_owned())
        .unwrap_or_default()
}

impl Header {
    /// The header of a run of `kind` and `tier` on this machine and
    /// checkout, against the baseline at `baseline`.
    pub(crate) fn gather(kind: &str, tier: &str, baseline: &str, groups: &[&str]) -> Self {
        let (cpu, cores, os) = machine();
        let rustc = crate::output(Command::new("rustc").arg("--version").current_dir(ROOT))
            .map(|(out, _)| out.trim().to_owned())
            .unwrap_or_default();
        Self {
            kind: kind.to_owned(),
            tier: tier.to_owned(),
            candidate: git(&["rev-parse", "HEAD"]),
            dirty: !git(&["status", "--porcelain"]).is_empty(),
            baseline: baseline.to_owned(),
            date: now(),
            cpu,
            cores,
            os,
            rustc,
            groups: groups.iter().map(|&g| g.to_owned()).collect(),
        }
    }

    /// The header as markdown list items.
    pub(crate) fn markdown(&self) -> String {
        let mut out = String::new();
        let dirty = if self.dirty {
            ", with uncommitted changes"
        } else {
            ""
        };
        let _ = writeln!(out, "- Candidate: {}{dirty}", short(&self.candidate));
        if !self.baseline.is_empty() {
            let _ = writeln!(out, "- Baseline: {}", short(&self.baseline));
        }
        if !self.tier.is_empty() {
            let _ = writeln!(
                out,
                "- Tier: {}; groups: {}",
                self.tier,
                self.groups.join(", ")
            );
        }
        let _ = writeln!(
            out,
            "- Machine: {}, {} cores, {}",
            self.cpu, self.cores, self.os
        );
        let _ = writeln!(out, "- {}; {}", self.rustc, self.date);
        out
    }

    fn to_json(&self) -> Value {
        json!({
            "kind": self.kind,
            "tier": self.tier,
            "candidate": { "commit": self.candidate, "dirty": self.dirty },
            "baseline": self.baseline,
            "date": self.date,
            "cpu": self.cpu,
            "cores": self.cores,
            "os": self.os,
            "rustc": self.rustc,
            "groups": self.groups,
        })
    }

    fn from_json(v: &Value) -> Result<Self> {
        let text = |key: &str| -> Result<String> {
            v[key]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("results.json: header has no {key}"))
        };
        Ok(Self {
            kind: text("kind")?,
            tier: text("tier")?,
            candidate: v["candidate"]["commit"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            dirty: v["candidate"]["dirty"].as_bool().unwrap_or_default(),
            baseline: text("baseline")?,
            date: text("date")?,
            cpu: text("cpu")?,
            cores: v["cores"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or_default(),
            os: text("os")?,
            rustc: text("rustc")?,
            groups: strings(&v["groups"]),
        })
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect()
}

impl Results {
    pub(crate) fn to_json(&self) -> Value {
        let metrics: Vec<Value> = self
            .metrics
            .iter()
            .map(|m| {
                let series: serde_json::Map<String, Value> = m
                    .series
                    .iter()
                    .map(|(name, values)| (name.clone(), json!(values)))
                    .collect();
                json!({
                    "name": m.name,
                    "unit": m.unit,
                    "values": series,
                    "result": m.result,
                })
            })
            .collect();
        let sections: Vec<Value> = self
            .sections
            .iter()
            .map(|s| {
                json!({
                    "title": s.title,
                    "note": s.note,
                    "columns": s.columns,
                    "rows": s.rows,
                    "after": s.after,
                })
            })
            .collect();
        json!({
            "header": self.header.to_json(),
            "metrics": metrics,
            "sections": sections,
        })
    }

    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        let text = |v: &Value, key: &str| v[key].as_str().unwrap_or_default().to_owned();
        let metrics = v["metrics"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| Entry {
                name: text(m, "name"),
                unit: text(m, "unit"),
                // serde_json keeps an object's keys sorted, so A comes
                // before B.
                series: m["values"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(name, values)| {
                        let values = values
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_f64)
                            .collect();
                        (name.clone(), values)
                    })
                    .collect(),
                result: text(m, "result"),
            })
            .collect();
        let sections = v["sections"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| Section {
                title: text(s, "title"),
                note: text(s, "note"),
                columns: strings(&s["columns"]),
                rows: s["rows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(strings)
                    .collect(),
                after: text(s, "after"),
            })
            .collect();
        Ok(Self {
            header: Header::from_json(&v["header"])?,
            metrics,
            sections,
        })
    }

    /// Writes `results.json`, and `summary` as `summary.md`, into `dir`.
    pub(crate) fn write(&self, dir: &Path, summary: &str) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.to_json()).map_err(|e| e.to_string())?;
        for (name, text) in [("results.json", json.as_str()), ("summary.md", summary)] {
            let path = dir.join(name);
            fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        eprintln!("results: {}", dir.display());
        Ok(())
    }

    pub(crate) fn read(dir: &Path) -> Result<Self> {
        let path = dir.join("results.json");
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_json(&v)
    }
}

/// A new, empty directory for a run's results under `target/perf/runs`,
/// named after the local time.
pub(crate) fn run_dir() -> Result<PathBuf> {
    let runs = Path::new(ROOT).join("target/perf/runs");
    let stamp = date(&["+%Y%m%d-%H%M%S"]);
    let mut dir = runs.join(&stamp);
    let mut n = 1;
    while dir.exists() {
        n += 1;
        dir = runs.join(format!("{stamp}-{n}"));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(fs::canonicalize(&dir).unwrap_or(dir))
}

/// The first 12 characters of a commit.
pub(crate) fn short(commit: &str) -> String {
    commit.chars().take(12).collect()
}

/// The CPU's model, the cores available, and the OS.
pub(crate) fn machine() -> (String, usize, String) {
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let run = |program: &str, args: &[&str]| {
        crate::output(Command::new(program).args(args))
            .map(|(out, _)| out.trim().to_owned())
            .unwrap_or_default()
    };
    let uname = run("uname", &["-sr"]);
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpu = cpuinfo
        .lines()
        .find_map(|l| Some(l.strip_prefix("model name")?.split_once(':')?.1.trim()))
        .map_or_else(
            || run("sysctl", &["-n", "machdep.cpu.brand_string"]),
            str::to_owned,
        );
    let release = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let os = release
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME=\"")?.strip_suffix('"'))
        .map_or_else(
            || {
                let mac = run("sw_vers", &["-productVersion"]);
                if mac.is_empty() {
                    uname.clone()
                } else {
                    format!("macOS {mac}, {uname}")
                }
            },
            |os| format!("{os}, {uname}"),
        );
    (cpu, cores, os)
}

/// `v` in `unit`: four significant digits in the largest unit under the
/// value for times and bytes, a count as is, an instruction count in
/// scientific notation.
pub(crate) fn show(v: f64, unit: &str) -> String {
    let scales: &[(f64, &str)] = match unit {
        "ns" => &[(1e9, "s"), (1e6, "ms"), (1e3, "µs"), (1.0, "ns")],
        "B" => &[(1e9, "GB"), (1e6, "MB"), (1e3, "KB"), (1.0, "B")],
        "instructions" => return format!("{v:.3e}"),
        "ms" | "s" | "KB" => return format!("{v:.1}{unit}"),
        _ => return format!("{v}"),
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

/// A section as a markdown table under its title and note.
fn section_markdown(s: &Section) -> String {
    let mut out = format!("## {}\n\n", s.title);
    if !s.note.is_empty() {
        let _ = write!(out, "{}\n\n", s.note);
    }
    let _ = writeln!(out, "| {} |", s.columns.join(" | "));
    let _ = writeln!(out, "|{}", "---|".repeat(s.columns.len()));
    for row in &s.rows {
        let _ = writeln!(out, "| {} |", row.join(" | "));
    }
    if !s.after.is_empty() {
        let _ = write!(out, "\n{}\n", s.after);
    }
    out
}

/// The doc-ready markdown of a run: a comparison's tables as
/// `docs/benchmarks.md` has them, or an A/B run's metrics before and after.
pub(crate) fn report(results: &Results) -> String {
    if !results.sections.is_empty() {
        let tables: Vec<String> = results.sections.iter().map(section_markdown).collect();
        return tables.join("\n");
    }
    let h = &results.header;
    let dirty = if h.dirty { ", uncommitted changes" } else { "" };
    let mut out = format!(
        "| Metric | Before ({}) | After ({}{dirty}) | Change | Result |\n|---|---|---|---|---|\n",
        short(&h.baseline),
        short(&h.candidate),
    );
    for m in &results.metrics {
        let side = |name: &str| {
            m.series
                .iter()
                .find(|(n, _)| n == name)
                .and_then(|(_, v)| median(&mut v.clone()))
        };
        let (before, after) = (side("A"), side("B"));
        let cell = |v: Option<f64>| v.map_or_else(|| "—".to_owned(), |v| show(v, &m.unit));
        let change = match (before, after) {
            (Some(a), Some(b)) if a > 0.0 => format!("{:+.1}%", (b - a) / a * 100.0),
            _ => "—".to_owned(),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {change} | {} |",
            m.name,
            cell(before),
            cell(after),
            m.result
        );
    }
    out
}

pub(crate) fn main(args: &[String]) -> Result<()> {
    let [dir] = args else {
        return Err("usage: cargo xtask report <run-dir>".to_owned());
    };
    print!("{}", report(&Results::read(&crate::expand_tilde(dir))?));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: &str) -> Header {
        Header {
            kind: kind.to_owned(),
            tier: if kind == "perf" { "full" } else { "" }.to_owned(),
            candidate: "4071802c6a1e0e2d9f0b".to_owned(),
            dirty: true,
            baseline: if kind == "perf" {
                "9fcc921ff8270000aaaa"
            } else {
                ""
            }
            .to_owned(),
            date: "2026-11-16 10:00 UTC".to_owned(),
            cpu: "AMD EPYC 9554P".to_owned(),
            cores: 4,
            os: "Ubuntu 24.04.5 LTS, Linux 6.12.115".to_owned(),
            rustc: "rustc 1.100.0".to_owned(),
            groups: vec!["frame".to_owned(), "print".to_owned()],
        }
    }

    fn perf_run() -> Results {
        Results {
            header: header("perf"),
            metrics: vec![
                Entry {
                    name: "print/range".to_owned(),
                    unit: "ns".to_owned(),
                    series: vec![
                        ("A".to_owned(), vec![17.2e9, 17.0e9, 17.1e9]),
                        ("B".to_owned(), vec![16.4e9, 16.2e9, 16.3e9]),
                    ],
                    result: "PASS".to_owned(),
                },
                Entry {
                    name: "frame/tree_frame alloc count".to_owned(),
                    unit: "count".to_owned(),
                    series: vec![
                        ("A".to_owned(), vec![469.0; 3]),
                        ("B".to_owned(), vec![470.0; 3]),
                    ],
                    result: "FAIL".to_owned(),
                },
            ],
            sections: Vec::new(),
        }
    }

    #[test]
    fn results_json_round_trips() {
        let mut compare = perf_run();
        compare.header = header("compare");
        compare.metrics[0].series = vec![("runs".to_owned(), vec![12.5, 13.0])];
        compare.sections = vec![Section {
            title: "The range's file list, with rename detection".to_owned(),
            note: String::new(),
            columns: vec!["Tool".to_owned(), "Command".to_owned()],
            rows: vec![vec!["dffy".to_owned(), "`dffy --name-status`".to_owned()]],
            after: "Both lists are identical.".to_owned(),
        }];
        for run in [perf_run(), compare] {
            let text = serde_json::to_string(&run.to_json()).unwrap();
            let back = Results::from_json(&serde_json::from_str(&text).unwrap()).unwrap();
            assert_eq!(back, run);
        }
    }

    #[test]
    fn report_of_an_ab_run_is_a_before_and_after_table() {
        assert_eq!(
            report(&perf_run()),
            "\
| Metric | Before (9fcc921ff827) | After (4071802c6a1e, uncommitted changes) | Change | Result |
|---|---|---|---|---|
| print/range | 17.10 s | 16.30 s | -4.7% | PASS |
| frame/tree_frame alloc count | 469 | 470 | +0.2% | FAIL |
"
        );
    }

    #[test]
    fn report_of_a_comparison_is_its_tables() {
        let mut run = perf_run();
        run.header = header("compare");
        run.sections = vec![
            Section {
                title: "One large file".to_owned(),
                note: String::new(),
                columns: vec!["Tool".to_owned(), "Total".to_owned()],
                rows: vec![vec!["GNU diff".to_owned(), "9.1ms".to_owned()]],
                after: String::new(),
            },
            Section {
                title: "Interactive viewers: as a pager".to_owned(),
                note: "The viewer reads the patch on stdin.".to_owned(),
                columns: vec!["Viewer".to_owned(), "First frame".to_owned()],
                rows: vec![vec!["dffy".to_owned(), "216.7ms (198.1–231.0)".to_owned()]],
                after: "Both first frames wait for git.".to_owned(),
            },
        ];
        assert_eq!(
            report(&run),
            "\
## One large file

| Tool | Total |
|---|---|
| GNU diff | 9.1ms |

## Interactive viewers: as a pager

The viewer reads the patch on stdin.

| Viewer | First frame |
|---|---|
| dffy | 216.7ms (198.1–231.0) |

Both first frames wait for git.
"
        );
    }
}
