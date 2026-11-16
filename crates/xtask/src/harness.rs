//! What hyperfine can't measure, for `cargo xtask compare`: the time to a
//! pipeline's first byte, the memory of each program in a command, and an
//! interactive viewer's first frame in a pseudo-terminal.
//!
//! Memory is sampled from `/proc/<pid>/status` every 2ms, on a thread, for
//! every process in the command's process group, keeping the peak per
//! program name: `RssAnon` is the program's own memory (heap, stacks),
//! `RssFile` the file pages it has mapped (its executable, and for git and
//! dffy the repository's pack files, which are page cache shared with the
//! OS). The shell that runs the command isn't reported, and neither is a
//! daemon that leaves the group. Without `/proc` (macOS) the memory of a
//! command run to completion is its peak RSS from `/usr/bin/time`, and a
//! viewer's isn't measured.

use std::collections::HashMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::num::NonZero;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::{Result, Sandbox, TIME, describe, peak_rss, time_flag};

pub(crate) const ROWS: u16 = 60;
pub(crate) const COLS: u16 = 200;
/// Seconds of silence that end a viewer's run.
const SETTLE: Duration = Duration::from_secs(1);
pub(crate) const TIMEOUT: Duration = Duration::from_secs(60);
/// How long after its first frame a viewer that never goes silent is given.
pub(crate) const BUSY: Duration = Duration::from_secs(5);

/// Peak `RssAnon` and `RssFile` in KB per program name.
pub(crate) type Peaks = HashMap<String, (f64, f64)>;

pub(crate) enum Memory {
    /// Per run, sampled from `/proc`.
    Programs(Vec<Peaks>),
    /// Per run, the whole command's peak RSS in KB.
    Peak(Vec<f64>),
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `kill -SIGNAL` to the process group `pgid`.
fn kill_group(signal: &str, pgid: u32) {
    Command::new("kill")
        .args([&format!("-{signal}"), "--", &format!("-{pgid}")])
        .stderr(Stdio::null())
        .status()
        .ok();
}

/// Milliseconds to the first byte `command` writes to its stdout, per run.
/// The pipeline is killed once that byte arrives: hyperfine has the total.
pub(crate) fn first_byte(sandbox: &Sandbox, runs: usize, command: &str) -> Result<Vec<f64>> {
    let mut times = Vec::new();
    for _ in 0..runs {
        sandbox.empty_dffy_cache()?;
        let mut cmd = sandbox.command("sh");
        cmd.args(["-c", command])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let start = Instant::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("{}: {e}", describe(&cmd)))?;
        let mut buf = [0; 1];
        let read = child
            .stdout
            .take()
            .map_or(0, |mut out| out.read(&mut buf).unwrap_or(0));
        let elapsed = start.elapsed();
        kill_group("KILL", child.id());
        child.wait().ok();
        if read == 0 {
            return Err(format!("no output: {command}"));
        }
        times.push(ms(elapsed));
    }
    Ok(times)
}

/// The memory of `command` run to completion, output to /dev/null.
pub(crate) fn memory(sandbox: &Sandbox, runs: usize, command: &str) -> Result<Memory> {
    if !cfg!(target_os = "linux") {
        let mut peaks = Vec::new();
        for _ in 0..runs {
            sandbox.empty_dffy_cache()?;
            let mut cmd = sandbox.command(TIME);
            cmd.args([time_flag(), "sh", "-c", command])
                .stdout(Stdio::null());
            let out = cmd
                .output()
                .map_err(|e| format!("{}: {e}", describe(&cmd)))?;
            let bytes = peak_rss(&String::from_utf8_lossy(&out.stderr))
                .ok_or_else(|| format!("{}: no peak RSS in its output", describe(&cmd)))?;
            peaks.push(bytes / 1024.0);
        }
        return Ok(Memory::Peak(peaks));
    }
    let mut peaks = Vec::new();
    for _ in 0..runs {
        sandbox.empty_dffy_cache()?;
        let sampler = Sampler::start();
        // The command waits for a line on stdin until the sampler watches
        // its group, so a command over in a few milliseconds is still seen.
        let mut cmd = sandbox.command("sh");
        cmd.args(["-c", r#"read _; exec </dev/null; eval "$1""#, "sh", command])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("{}: {e}", describe(&cmd)))?;
        sampler.watch(child.id());
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(b"\n").ok();
        }
        child
            .wait()
            .map_err(|e| format!("{}: {e}", describe(&cmd)))?;
        peaks.push(sampler.stop());
    }
    Ok(Memory::Programs(peaks))
}

struct Sampler {
    group: mpsc::Sender<u32>,
    thread: JoinHandle<Peaks>,
}

impl Sampler {
    /// Starts before the command so its process isn't mistaken for one
    /// that was running already.
    fn start() -> Self {
        let mut members: HashMap<u32, bool> = pids().map(|pid| (pid, false)).collect();
        let (group, watch) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut peaks = Peaks::new();
            let Ok(pgid) = watch.recv() else { return peaks };
            if !cfg!(target_os = "linux") {
                return peaks;
            }
            // Back to back at first, so a command that's over in a few
            // milliseconds is still seen.
            let eager = Instant::now() + Duration::from_millis(100);
            loop {
                sample(pgid, &mut members, &mut peaks);
                let wait = if Instant::now() < eager {
                    Duration::ZERO
                } else {
                    Duration::from_millis(2)
                };
                if watch.recv_timeout(wait) != Err(RecvTimeoutError::Timeout) {
                    break;
                }
            }
            sample(pgid, &mut members, &mut peaks);
            peaks
        });
        Self { group, thread }
    }

    fn watch(&self, pgid: u32) {
        self.group.send(pgid).ok();
    }

    fn stop(self) -> Peaks {
        drop(self.group);
        self.thread.join().unwrap_or_default()
    }
}

fn pids() -> impl Iterator<Item = u32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
}

fn sample(pgid: u32, members: &mut HashMap<u32, bool>, peaks: &mut Peaks) {
    for pid in pids() {
        let member = *members
            .entry(pid)
            .or_insert_with(|| process_group(pid) == Some(pgid));
        if !member {
            continue;
        }
        let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
            continue;
        };
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
                .map(str::trim)
        };
        let kb = |v: &str| v.split_whitespace().next().and_then(|n| n.parse().ok());
        let (Some(name), Some(anon), Some(mapped)) = (
            field("Name"),
            field("RssAnon").and_then(kb),
            field("RssFile").and_then(kb),
        ) else {
            continue;
        };
        if name == "sh" {
            continue;
        }
        let peak: &mut (f64, f64) = peaks.entry(name.to_owned()).or_default();
        *peak = (peak.0.max(anon), peak.1.max(mapped));
    }
}

fn process_group(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesized name: state, ppid, pgrp.
    stat[stat.rfind(')')? + 2..]
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

/// One run of a viewer in the pty.
pub(crate) enum Run {
    /// The match text never showed within the timeout.
    Timeout,
    /// The refusal text showed instead.
    Refused,
    /// Milliseconds from start to the first byte, to the match on screen,
    /// and to the last byte before a second of silence; `None` for a viewer
    /// still drawing [`BUSY`] after its first frame.
    Shown(f64, f64, Option<f64>),
}

/// Runs `argv` in a `COLS`×`ROWS` pty until `matched` is on its screen, for
/// up to `runs` runs: a timeout or refusal ends them, since more would only
/// repeat it.
pub(crate) fn pty(
    sandbox: &Sandbox,
    runs: usize,
    argv: &[&str],
    matched: &str,
    refusal: &str,
) -> Result<(Vec<Run>, Vec<Peaks>)> {
    let (mut results, mut peaks) = (Vec::new(), Vec::new());
    for _ in 0..runs {
        sandbox.empty_dffy_cache()?;
        let (run, peak) = pty_run(sandbox, argv, matched, refusal)?;
        peaks.push(peak);
        let done = !matches!(run, Run::Shown(..));
        results.push(run);
        if done {
            break;
        }
    }
    Ok((results, peaks))
}

/// Output chunks are timestamped by the thread that reads them, so the
/// terminal emulator's time never delays a timestamp. A run ends at the
/// first second of silence once the text is on screen, [`BUSY`] after it
/// for a viewer that keeps drawing, at [`TIMEOUT`] when it never shows, or
/// at the first second of silence with the refusal text on screen instead.
fn pty_run(sandbox: &Sandbox, argv: &[&str], matched: &str, refusal: &str) -> Result<(Run, Peaks)> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;
    let mut cmd = CommandBuilder::new(argv[0]);
    cmd.args(&argv[1..]);
    cmd.env_clear();
    for (var, value) in sandbox.vars() {
        cmd.env(var, value);
    }
    cmd.cwd(&sandbox.repo);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("pty: {e}"))?;
    let mut writer = pair.master.take_writer().map_err(|e| format!("pty: {e}"))?;

    let sampler = Sampler::start();
    let start = Instant::now();
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("{}: {e}", argv.join(" ")))?;
    drop(pair.slave);
    if let Some(pgid) = child.process_id() {
        sampler.watch(pgid);
    }
    let (chunks, received) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = vec![0; 65536];
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            if chunks.send((Instant::now(), buf[..n].to_vec())).is_err() {
                break;
            }
        }
    });

    let size = |n: u16| NonZero::new(n).ok_or("a pty needs rows and columns");
    let mut screen = vt100::Parser::new(size(ROWS)?, size(COLS)?, 0);
    let on_screen = |screen: &vt100::Parser, text: &str| {
        screen.screen().rows(0, COLS).any(|row| row.contains(text))
    };
    let mut times: Vec<Instant> = Vec::new();
    let mut shown = None;
    let mut settled = false;
    let mut deadline = start + TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match received.recv_timeout(left.min(SETTLE)) {
            Ok((at, data)) => {
                for reply in replies(&data) {
                    writer.write_all(&reply).ok();
                }
                screen.process(&data);
                times.push(at);
                if shown.is_none() && on_screen(&screen, matched) {
                    shown = Some(times.len() - 1);
                    deadline = deadline.min(Instant::now() + BUSY);
                }
            }
            Err(RecvTimeoutError::Timeout) if left > SETTLE => {
                if shown.is_some() {
                    settled = true;
                    break;
                }
                if !refusal.is_empty() && on_screen(&screen, refusal) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                settled = true;
                break;
            }
        }
    }

    end(&mut *child, &mut writer, &received, shown.is_some());
    let since = |at: Instant| ms(at.duration_since(start));
    let run = match shown {
        Some(i) => Run::Shown(
            since(times[0]),
            since(times[i]),
            settled.then(|| since(times.last().copied().unwrap_or(start))),
        ),
        None if !refusal.is_empty() && on_screen(&screen, refusal) => Run::Refused,
        None => Run::Timeout,
    };
    Ok((run, sampler.stop()))
}

/// Quits a viewer that showed its frame with `q`, as a user would, and
/// ends its group if it's still running 5s later; kills one that didn't.
fn end(
    child: &mut dyn portable_pty::Child,
    writer: &mut dyn std::io::Write,
    received: &mpsc::Receiver<(Instant, Vec<u8>)>,
    shown: bool,
) {
    let running = |child: &mut dyn portable_pty::Child| child.try_wait().ok().flatten().is_none();
    if shown {
        writer.write_all(b"q").ok();
        let deadline = Instant::now() + Duration::from_secs(5);
        while running(child) && Instant::now() < deadline {
            received.recv_timeout(Duration::from_millis(50)).ok();
        }
    }
    if running(child)
        && let Some(pgid) = child.process_id()
    {
        kill_group(if shown { "TERM" } else { "KILL" }, pgid);
    }
    child.wait().ok();
}

/// Terminal queries with the replies a dark xterm-like terminal of
/// `COLS`×`ROWS` gives, so a viewer that waits on one isn't charged for a
/// terminal that never replies.
const REPLIES: [(&[u8], &[u8]); 13] = [
    (b"\x1b[c", b"\x1b[?62;22c"),
    (b"\x1b[0c", b"\x1b[?62;22c"),
    (b"\x1b[6n", b"\x1b[1;1R"),
    (b"\x1b]10;?\x07", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
    (b"\x1b]10;?\x1b\\", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
    (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
    (b"\x1b]11;?\x1b\\", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
    (b"\x1b[?u", b"\x1b[?0u"),
    (b"\x1b[>q", b"\x1bP>|harness\x1b\\"),
    (b"\x1b[>0q", b"\x1bP>|harness\x1b\\"),
    (b"\x1b[14t", b"\x1b[4;1200;2000t"),
    (b"\x1b[16t", b"\x1b[6;20;10t"),
    (b"\x1b[18t", b"\x1b[8;60;200t"),
];

/// The replies to the queries in `data`. A query split across two reads
/// goes unanswered.
fn replies(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (i, _) in data.iter().enumerate().filter(|&(_, &b)| b == 0x1b) {
        let rest = &data[i..];
        if let Some((_, reply)) = REPLIES.iter().find(|(query, _)| rest.starts_with(query)) {
            out.push(reply.to_vec());
        } else if let Some(mode) = rest.strip_prefix(b"\x1b[?") {
            // DECRQM: every mode reads as reset.
            let digits = mode.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 0 && mode[digits..].starts_with(b"$p") {
                out.push([b"\x1b[?", &mode[..digits], b";2$y"].concat());
            }
        }
    }
    out
}
