//! The `comments` group of `cargo xtask perf`: the viewer on the kernel's
//! v6.0..v6.1 with a comments file of review threads, timed in a 200×60
//! pseudo-terminal from its start, through `m` over the threads, to paging
//! through them. It opens two reviews ([`SCENARIOS`]): 300 threads over 3
//! files, and a large pull request's 2,000 over 16.
//!
//! Each review's comments file is a fixture: written once per measurement
//! through the candidate's own `dffy comments apply`, from the range's diff
//! and the commented files' text at v6.1, so both sides open the same file
//! and its threads are the same on every run. Thread `i`'s first comment
//! starts with `bench-<i>`, which the script waits for on screen; every
//! third thread spans up to three lines, every fifth is resolved, every
//! tenth is unplaced (its anchor's lines aren't in the file), and thread
//! `i` has `i % 4` replies.

use std::io::{Read as _, Write as _};
use std::num::NonZero;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};

use crate::harness::{self, COLS, ROWS, TIMEOUT};
use crate::{Result, Sandbox, describe, median};

/// A review the group opens.
struct Scenario {
    /// What its rows' names put between `comments/` and the metric's.
    prefix: &'static str,
    /// Its comments file's name in the sandbox's root, which survives the
    /// sandbox's reset before every run.
    file: &'static str,
    /// The files with threads, in the viewer's order, and each one's
    /// threads. Each starts with `.clang-format`, the file the viewer opens
    /// on, whose one thread is on its only changed line, so the first
    /// screen shows a thread; `m` goes from there to the next file, where
    /// the paging runs.
    files: &'static [(&'static str, usize)],
    /// Whether it has a `settled` row.
    settled: bool,
}

const SCENARIOS: [Scenario; 2] = [
    Scenario {
        prefix: "",
        file: "comments.json",
        // `mm/vmscan.c`, with 141 hunks and 3,221 added lines, holds most.
        files: &[
            (".clang-format", 1),
            ("mm/vmscan.c", 250),
            ("net/core/filter.c", 49),
        ],
        settled: true,
    },
    Scenario {
        prefix: "large ",
        file: "comments-large.json",
        // C files edited in the range with 695 to 3,221 added lines, about
        // a thread per ten of them.
        files: &[
            (".clang-format", 1),
            ("drivers/clk/clk_test.c", 160),
            ("drivers/gpu/drm/amd/amdgpu/amdgpu_cs.c", 80),
            ("drivers/gpu/drm/vc4/vc4_hdmi.c", 80),
            ("drivers/hwmon/f71882fg.c", 150),
            ("drivers/iio/pressure/bmp280-core.c", 80),
            (
                "drivers/net/ethernet/marvell/prestera/prestera_router.c",
                110,
            ),
            ("drivers/net/wireless/ralink/rt2x00/rt2800lib.c", 170),
            ("drivers/net/wireless/realtek/rtw89/coex.c", 160),
            ("drivers/vfio/pci/vfio_pci_core.c", 70),
            ("fs/btrfs/tree-log.c", 110),
            ("fs/ext4/super.c", 70),
            ("mm/khugepaged.c", 80),
            ("mm/mmap.c", 120),
            ("mm/vmscan.c", 400),
            ("tools/testing/memblock/tests/alloc_nid_api.c", 159),
        ],
        // The first screen is the same, so its settling is the other's.
        settled: false,
    },
];

/// Runs a round takes the median of.
const RUNS: usize = 3;

/// `m` presses, then page-downs, per run. The first two `m` focus the
/// thread on `.clang-format` and then go to the first in the next file;
/// sixteen pages from there stay well inside it.
const THREAD_KEYS: usize = 16;
const PAGE_KEYS: usize = 16;

const PAGE_DOWN: &[u8] = b"\x1b[6~";

/// The end of a synchronized update closes every frame dffy draws.
const FRAME_END: &[u8] = b"\x1b[?2026l";

/// Silence that ends the opening, as for the viewer tables.
const SETTLE: Duration = Duration::from_secs(1);

/// Silence after a key's frame before the next key, so a late repaint,
/// such as highlighting arriving, isn't taken for the next key's frame.
const QUIET: Duration = Duration::from_millis(150);

/// How long a key's frame, and the screen it should show, may take.
const KEY_TIMEOUT: Duration = Duration::from_secs(10);

/// The metrics of one run, in ms, each named as its row after `comments/`
/// and its scenario's prefix.
type Times = [(&'static str, f64); 5];

const AUTHORS: [&str; 4] = ["", "octocat", "hubot", "monalisa"];

/// Writes every scenario's fixture with `dffy`'s `comments apply` in the
/// sandbox's repository, replacing earlier ones.
pub(crate) fn fixture(sandbox: &Sandbox, dffy: &Path) -> Result<()> {
    SCENARIOS.iter().try_for_each(|s| write(sandbox, dffy, s))
}

fn write(sandbox: &Sandbox, dffy: &Path, scenario: &Scenario) -> Result<()> {
    let file = sandbox.path(scenario.file);
    let lock = sandbox.path(&format!("{}.lock", scenario.file));
    for stale in [file.clone(), lock] {
        match std::fs::remove_file(&stale) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("{}: {e}", stale.display()));
            }
            _ => {}
        }
    }
    let mut targets = Vec::new();
    for &(path, threads) in scenario.files {
        let git = |args: &[&str]| crate::output(sandbox.command("git").args(args)).map(|(o, _)| o);
        let diff = git(&[
            "diff",
            "--no-ext-diff",
            "--no-renames",
            "-U0",
            "v6.0",
            "v6.1",
            "--",
            path,
        ])?;
        let text = git(&["show", &format!("v6.1:{path}")])?;
        let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        if text.ends_with('\n') {
            lines.pop();
        }
        targets.push(Target {
            path,
            lines,
            hunks: added(&diff),
            threads,
        });
    }
    let roots = roots(&targets)?;
    let ids = apply(sandbox, dffy, &file, &roots)?;
    if ids.len() != roots.len() {
        return Err(format!(
            "{}: {} ids for {} threads",
            file.display(),
            ids.len(),
            roots.len()
        ));
    }
    apply(sandbox, dffy, &file, &replies(&ids))?;
    Ok(())
}

/// Whether `dffy` reads the comments file `file`, an absolute path: a build
/// from before comments doesn't know `--comments`. A file that doesn't
/// exist reads as empty.
fn opens(dffy: &Path, file: &Path) -> bool {
    let mut cmd = Command::new(dffy);
    cmd.arg("--comments").arg(file).args(["comments", "--json"]);
    crate::output(&mut cmd).is_ok()
}

/// [`opens`] for every scenario's file in `dir`: what the comments group
/// needs of a build ([`crate::ab::Needs`]).
pub(crate) fn opens_in(dffy: &Path, dir: &Path) -> bool {
    SCENARIOS.iter().all(|s| opens(dffy, &dir.join(s.file)))
}

/// A file the threads go on.
struct Target {
    path: &'static str,
    /// Its text at v6.1, a line per entry without its `\n`.
    lines: Vec<String>,
    /// Its hunks' added lines on the new side: the first, 1-based, and how
    /// many.
    hunks: Vec<(usize, usize)>,
    threads: usize,
}

/// The new side's added lines of each hunk of a `-U0` diff, as
/// `(first, count)`; hunks that only remove lines are left out.
fn added(diff: &str) -> Vec<(usize, usize)> {
    diff.lines()
        .filter_map(|line| {
            let new = line
                .strip_prefix("@@ -")?
                .split(' ')
                .nth(1)?
                .strip_prefix('+')?;
            let (first, count) = new.split_once(',').unwrap_or((new, "1"));
            Some((first.parse().ok()?, count.parse().ok()?))
        })
        .filter(|&(_, count)| count > 0)
        .collect()
}

fn token(i: usize) -> String {
    format!("bench-{i:03}")
}

/// Whether thread `i`'s anchor has lines that aren't in its file.
fn unplaced(i: usize) -> bool {
    i % 10 == 7
}

fn text(i: usize) -> String {
    let token = token(i);
    match i % 3 {
        0 => format!("{token}: this can overflow."),
        1 => format!(
            "{token}: the reclaim loop takes the LRU lock again on this path after \
             dropping it for writeback, so a concurrent `shrink_folio_list` can see the \
             list half moved; either hold it across the move or recheck the folio's \
             state once it's taken back."
        ),
        _ => format!(
            "{token}: two things here:\n\n- the lock is taken twice on this path\n\
             - `nr_scanned` isn't reset between passes\n\n\
             ```c\nspin_lock_irq(&lruvec->lru_lock);\n```"
        ),
    }
}

fn created(i: usize) -> String {
    format!("2026-11-17T{:02}:{:02}:00Z", 8 + i / 60 % 12, i % 60)
}

/// The `add` operations of the threads' first comments, numbered across
/// `targets` in order, spread evenly over each file's added lines.
fn roots(targets: &[Target]) -> Result<Vec<Value>> {
    let mut ops = Vec::new();
    for t in targets {
        // Each added line, with the last added line of its hunk.
        let added: Vec<(usize, usize)> = t
            .hunks
            .iter()
            .flat_map(|&(first, n)| (first..first + n).map(move |l| (l, first + n - 1)))
            .collect();
        if added.len() < t.threads || added.last().is_some_and(|&(_, l)| l > t.lines.len()) {
            return Err(format!(
                "{}: {} added lines in {} for {} threads; is DFFY_BENCH_REPO the kernel?",
                t.path,
                added.len(),
                t.lines.len(),
                t.threads
            ));
        }
        for k in 0..t.threads {
            let i = ops.len();
            let (start, last) = added[k * added.len() / t.threads];
            let end = if i % 3 == 1 {
                (start + 2).min(last)
            } else {
                start
            };
            let line = |n: usize| t.lines[n - 1].clone();
            let lines: Vec<String> = if unplaced(i) {
                vec![format!("{}: a line that isn't in the file", token(i))]
            } else {
                (start..=end).map(line).collect()
            };
            let before: Vec<String> = (start.saturating_sub(2).max(1)..start).map(line).collect();
            let after: Vec<String> = (end + 1..=(end + 2).min(t.lines.len())).map(line).collect();
            ops.push(json!({"op": "add", "comment": {
                "path": t.path,
                "side": "new",
                "start": {"old": null, "new": start},
                "end": {"old": null, "new": end},
                "text": text(i),
                "author": AUTHORS[i % AUTHORS.len()],
                "created": created(i),
                "resolved": i % 5 == 4,
                "anchor": {"before": before, "lines": lines, "after": after},
            }}));
        }
    }
    Ok(ops)
}

/// The replies to the threads whose first comments got `ids`.
fn replies(ids: &[String]) -> Vec<Value> {
    ids.iter()
        .enumerate()
        .flat_map(|(i, id)| {
            (1..=i % 4).map(move |j| {
                json!({"op": "add", "comment": {
                    "in_reply_to": id,
                    "text": format!("{} reply {j}: agreed, fixed in the next push.", token(i)),
                    "author": AUTHORS[(i + j) % AUTHORS.len()],
                    "created": created(i + j),
                }})
            })
        })
        .collect()
}

/// `ops` applied to `file` by `dffy comments apply`: the ids it answers.
fn apply(sandbox: &Sandbox, dffy: &Path, file: &Path, ops: &[Value]) -> Result<Vec<String>> {
    let mut cmd = sandbox.command(dffy);
    cmd.arg("--comments")
        .arg(file)
        .args(["comments", "apply"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{}: {e}", describe(&cmd)))?;
    // A dffy that exits before reading it all says why on stderr, which
    // matters more than the broken pipe.
    let wrote = child.stdin.take().map_or(Ok(()), |mut stdin| {
        stdin.write_all(Value::from(ops.to_vec()).to_string().as_bytes())
    });
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{}: {e}", describe(&cmd)))?;
    if !out.status.success() {
        return Err(format!(
            "{}: {}\n{}",
            describe(&cmd),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim_end()
        ));
    }
    wrote.map_err(|e| format!("{}: {e}", describe(&cmd)))?;
    let answer: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("{}: its answer: {e}", describe(&cmd)))?;
    Ok(answer["ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|id| Some(id.as_str()?.to_owned()))
        .collect())
}

/// Each scenario's metrics, in ms and named as their rows after
/// `comments/`: their medians over [`RUNS`] runs of `dffy` on its fixture.
pub(crate) fn measure(sandbox: &Sandbox, dffy: &Path) -> Result<Vec<(String, f64)>> {
    let mut times = Vec::new();
    for s in &SCENARIOS {
        let file = sandbox.path(s.file);
        let mut runs = Vec::new();
        for _ in 0..RUNS {
            runs.push(run(sandbox, dffy, &file).map_err(|e| format!("{}: {e}", s.file))?);
        }
        for (k, &(metric, _)) in runs[0].iter().enumerate() {
            if metric == "settled" && !s.settled {
                continue;
            }
            let mut values: Vec<f64> = runs.iter().map(|r| r[k].1).collect();
            let ms = median(&mut values).unwrap_or_default();
            times.push((format!("{}{metric}", s.prefix), ms));
        }
    }
    Ok(times)
}

/// One run: the time from start to the first frame, to the first thread
/// on screen and to the last output before a second of silence, then the
/// median time from a key to the end of the frame it draws, of `m` and of
/// page down.
fn run(sandbox: &Sandbox, dffy: &Path, file: &Path) -> Result<Times> {
    sandbox.reset()?;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;
    let mut cmd = CommandBuilder::new(dffy);
    cmd.arg("--comments");
    cmd.arg(file);
    cmd.arg("v6.0..v6.1");
    cmd.env_clear();
    for (var, value) in sandbox.vars(dffy.as_os_str()) {
        cmd.env(var, value);
    }
    cmd.cwd(&sandbox.repo);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("pty: {e}"))?;
    let size = |n: u16| NonZero::new(n).ok_or("a pty needs rows and columns");
    let mut term = Term {
        screen: vt100::Parser::new(size(ROWS)?, size(COLS)?, 0),
        writer: pair.master.take_writer().map_err(|e| format!("pty: {e}"))?,
        received: {
            let (chunks, received) = mpsc::channel();
            thread::spawn(move || {
                let mut buf = vec![0; 65536];
                while let Ok(n @ 1..) = reader.read(&mut buf) {
                    if chunks.send((Instant::now(), buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            });
            received
        },
        tail: Vec::new(),
    };
    let start = Instant::now();
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("{}: {e}", dffy.display()))?;
    drop(pair.slave);
    let result = script(&mut term, start);
    harness::end(&mut *child, &mut *term.writer, &term.received, true);
    result
}

fn script(term: &mut Term, start: Instant) -> Result<Times> {
    let first = token(0);
    let (mut frame, mut thread, mut last) = (None, None, start);
    loop {
        let left = (start + TIMEOUT).saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!(
                "no {first} on screen within {TIMEOUT:?}; the screen:\n{}",
                term.text()
            ));
        }
        match term.chunk(left.min(SETTLE))? {
            Some((at, ended)) => {
                last = at;
                if ended && frame.is_none() {
                    frame = Some(at);
                }
                if thread.is_none() && term.shows(&first) {
                    thread = Some(at);
                }
            }
            None if frame.is_some() && thread.is_some() => break,
            None => {}
        }
    }
    let since = |at: Option<Instant>| ms(at.unwrap_or(start).duration_since(start));
    let mut threads = Vec::new();
    for _ in 0..THREAD_KEYS {
        threads.push(term.press(b"m", Some("bench-"))?);
    }
    // The paging is meant to run among the second file's threads. `m` walks
    // the placed threads in order, and the unplaced ones after them, under
    // their file's last row.
    let reached = (0..)
        .filter(|&i| !unplaced(i))
        .nth(THREAD_KEYS - 1)
        .map_or_else(String::new, token);
    if !term.shows(&reached) {
        return Err(format!(
            "{THREAD_KEYS} presses of m didn't reach {reached}; the screen:\n{}",
            term.text()
        ));
    }
    let mut pages = Vec::new();
    for _ in 0..PAGE_KEYS {
        pages.push(term.press(PAGE_DOWN, None)?);
    }
    Ok([
        ("first frame", since(frame)),
        ("first thread", since(thread)),
        ("settled", since(Some(last))),
        ("next thread", median(&mut threads).unwrap_or_default()),
        ("page down", median(&mut pages).unwrap_or_default()),
    ])
}

/// The viewer's side of the pty, read through a terminal emulator.
struct Term {
    screen: vt100::Parser,
    writer: Box<dyn std::io::Write + Send>,
    /// Output chunks, timestamped by the thread that reads them.
    received: mpsc::Receiver<(Instant, Vec<u8>)>,
    /// The last bytes read, for a frame end split across two chunks.
    tail: Vec<u8>,
}

impl Term {
    /// The next chunk within `wait`: when it was read, and whether a frame
    /// ended in it; none after `wait` of silence.
    fn chunk(&mut self, wait: Duration) -> Result<Option<(Instant, bool)>> {
        let (at, data) = match self.received.recv_timeout(wait) {
            Ok(chunk) => chunk,
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(format!("dffy exited; the screen:\n{}", self.text()));
            }
        };
        for reply in harness::replies(&data) {
            self.writer.write_all(&reply).ok();
        }
        self.screen.process(&data);
        let mut seen = std::mem::take(&mut self.tail);
        seen.extend_from_slice(&data);
        let ended = seen.windows(FRAME_END.len()).any(|w| w == FRAME_END);
        self.tail = seen.split_off(seen.len().saturating_sub(FRAME_END.len() - 1));
        Ok(Some((at, ended)))
    }

    /// Writes `key`, and returns the ms to the end of the first frame after
    /// it. Then reads on until `expect`, when given, is on screen and
    /// output pauses for [`QUIET`].
    fn press(&mut self, key: &[u8], expect: Option<&str>) -> Result<f64> {
        let sent = Instant::now();
        self.writer
            .write_all(key)
            .map_err(|e| format!("pty: {e}"))?;
        let name = String::from_utf8_lossy(key).escape_debug().to_string();
        let deadline = sent + KEY_TIMEOUT;
        let mut latency = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let shown = expect.is_none_or(|text| self.shows(text));
            let wait = if latency.is_some() && shown {
                QUIET.min(left)
            } else {
                left
            };
            match self.chunk(wait)? {
                Some((at, true)) if latency.is_none() && at >= sent => {
                    latency = Some(ms(at - sent));
                }
                Some(_) if !left.is_zero() => {}
                None if latency.is_some() && shown => break,
                _ => {
                    return Err(format!(
                        "{name}: {} within {KEY_TIMEOUT:?}; the screen:\n{}",
                        match (latency, expect) {
                            (None, _) => "no frame".to_owned(),
                            (Some(_), Some(text)) => format!("no {text} on screen"),
                            (Some(_), None) => "output never paused".to_owned(),
                        },
                        self.text()
                    ));
                }
            }
        }
        Ok(latency.unwrap_or_default())
    }

    fn shows(&self, text: &str) -> bool {
        self.screen
            .screen()
            .rows(0, COLS)
            .any(|row| row.contains(text))
    }

    fn text(&self) -> String {
        self.screen.screen().contents()
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_give_their_added_lines_and_skip_removals() {
        let diff = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -3 +3 @@ fn a()
-x
+y
@@ -10,2 +9,0 @@
-gone
-gone
@@ -20,0 +18,3 @@
+a
+b
+c
";
        assert_eq!(added(diff), [(3, 1), (18, 3)]);
    }

    fn target(
        path: &'static str,
        lines: usize,
        hunks: &[(usize, usize)],
        threads: usize,
    ) -> Target {
        Target {
            path,
            lines: (1..=lines).map(|n| format!("line {n}")).collect(),
            hunks: hunks.to_vec(),
            threads,
        }
    }

    #[test]
    fn the_fixture_is_the_same_every_time_with_every_kind_of_thread() {
        let targets = [
            target("a", 5, &[(2, 1)], 1),
            target("b", 400, &[(1, 100), (200, 150), (399, 2)], 39),
        ];
        let ops = roots(&targets).unwrap();
        assert_eq!(ops, roots(&targets).unwrap());
        assert_eq!(ops.len(), 40);
        let comment = |i: usize| &ops[i]["comment"];
        assert_eq!(comment(0)["path"], "a");
        assert_eq!(comment(0)["start"]["new"], 2);
        assert_eq!(
            comment(0)["anchor"],
            json!({"before": ["line 1"], "lines": ["line 2"], "after": ["line 3", "line 4"]})
        );
        let texts: Vec<&str> = ops
            .iter()
            .map(|op| op["comment"]["text"].as_str().unwrap())
            .collect();
        for (i, text) in texts.iter().enumerate() {
            assert!(text.starts_with(&token(i)), "{text}");
        }
        // Thread 1 spans three lines; 7 is unplaced; 4 is resolved.
        assert_eq!(comment(1)["anchor"]["lines"].as_array().unwrap().len(), 3);
        assert_eq!(
            comment(7)["anchor"]["lines"][0],
            "bench-007: a line that isn't in the file"
        );
        assert_eq!(comment(4)["resolved"], true);
        assert_eq!(comment(3)["resolved"], false);
        // Every commented line is an added one, and none is past the end.
        for op in &ops[1..] {
            let (start, end) = (&op["comment"]["start"]["new"], &op["comment"]["end"]["new"]);
            let (start, end) = (start.as_u64().unwrap(), end.as_u64().unwrap());
            assert!(
                (1..=100).contains(&start) || (200..350).contains(&start) || start >= 399,
                "{start}"
            );
            assert!(end >= start && end <= 400, "{start}-{end}");
        }
    }

    #[test]
    fn too_few_added_lines_is_an_error() {
        let err = roots(&[target("a", 5, &[(2, 1)], 2)]).unwrap_err();
        assert!(err.contains("1 added lines"), "{err}");
    }

    #[test]
    fn thread_i_gets_i_mod_4_replies() {
        let ids: Vec<String> = (0..8).map(|i| format!("id{i}")).collect();
        let ops = replies(&ids);
        assert_eq!(ops.len(), 2 * (1 + 2 + 3));
        assert_eq!(ops[0]["comment"]["in_reply_to"], "id1");
        assert!(ops.iter().all(|op| op["comment"].get("anchor").is_none()));
    }
}
