//! Watching through dffyd end to end. Subscribers are `dffy daemon watch`
//! processes, which go through the same client code as the viewer: connect,
//! start the broker if needed, fall back to watching alone. Each test gets
//! its own runtime directory, so its own broker.
//!
//! Gated tests: `DFFY_BENCH_REPO` (the kernel clone) runs the checkout
//! storm; `DFFY_SYSCTL_TESTS=1` runs the watch-limit test, which lowers
//! `fs.inotify.max_user_watches` through passwordless sudo for a moment.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dffy_core::git::{ChangeSet, Repo, Revisions};
use dffy_core::watch::{Change, MAX_BATCH, Root};

const TIMEOUT: Duration = Duration::from_secs(10);

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    for (path, text) in [
        ("a.txt", "a\n"),
        ("src/lib.rs", "fn f() {}\n"),
        ("docs/x/y.md", "y\n"),
    ] {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(full, text).expect("write");
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

/// A private runtime directory, so a private broker; stopped on drop.
struct Runtime {
    dir: tempfile::TempDir,
}

impl Runtime {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn dffy(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dffy"));
        cmd.args(args)
            .env("XDG_RUNTIME_DIR", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("DFFY_DAEMON_IDLE_SECS", "1");
        cmd
    }

    #[track_caller]
    fn status(&self) -> String {
        let out = self
            .dffy(&["--color=never", "daemon", "status"])
            .output()
            .expect("status");
        assert!(
            out.status.success(),
            "status: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[track_caller]
    fn broker_pid(&self) -> String {
        let status = self.status();
        status
            .split("pid ")
            .nth(1)
            .and_then(|s| s.split(',').next())
            .unwrap_or_else(|| panic!("no pid in {status}"))
            .to_owned()
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("dffy/broker-v1.sock")
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.dffy(&["daemon", "stop"]).output().ok();
    }
}

fn signal(pid: &str, signal: &str) {
    let status = Command::new("kill")
        .args([signal, pid])
        .status()
        .expect("kill");
    assert!(status.success(), "kill {signal} {pid}");
}

/// A `dffy daemon watch` process and its output lines, timestamped.
struct Subscriber {
    child: Child,
    lines: mpsc::Receiver<(Instant, String)>,
}

impl Subscriber {
    fn spawn(runtime: &Runtime, dir: &Path) -> Self {
        let mut child = runtime
            .dffy(&["daemon", "watch"])
            .arg(dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn subscriber");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send((Instant::now(), line)).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    /// The first line matching `want`, skipping others.
    fn wait_for(&self, what: &str, want: impl Fn(&str) -> bool) -> (Instant, String) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok((at, line)) if want(&line) => return (at, line),
                Ok(_) => {}
                Err(e) => panic!("waiting for {what}: {e}"),
            }
        }
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

#[test]
fn three_subscribers_share_one_watcher_and_carry_on_without_it() {
    let repo = repo();
    let root = repo.path().canonicalize().expect("canonical");
    let runtime = Runtime::new();
    let subscribers: Vec<Subscriber> = (0..3)
        .map(|_| {
            let s = Subscriber::spawn(&runtime, &root);
            s.wait_for("ready", |l| {
                l.starts_with("ready inotify") || l.starts_with("ready fsevents")
            });
            s
        })
        .collect();
    let status = runtime.status();
    assert!(status.contains("1 root\n"), "{status}");
    assert!(status.contains("3 subscribers"), "{status}");
    assert_eq!(
        status.matches("subscriber").count(),
        1,
        "one watcher: {status}"
    );

    let edited = root.join("a.txt");
    let written = Instant::now();
    std::fs::write(&edited, "edited\n").expect("write");
    let want = format!("changed {}", edited.display());
    for s in &subscribers {
        let (at, _) = s.wait_for("the edit", |l| l == want);
        eprintln!("broker delivered the edit in {:?}", at - written);
    }

    let pid = runtime.broker_pid();
    signal(&pid, "-KILL");
    for s in &subscribers {
        s.wait_for("the fallback note", |l| {
            l.starts_with("local dffyd went away")
        });
        s.wait_for("watching alone", |l| l.starts_with("ready"));
    }
    let created = root.join("docs/x/new.md");
    std::fs::write(&created, "new\n").expect("write");
    let want = format!("changed {}", created.display());
    for s in &subscribers {
        s.wait_for("the edit after the fallback", |l| l == want);
    }
    let gone = Instant::now();
    while Path::new(&format!("/proc/{pid}")).exists() && gone.elapsed() < TIMEOUT {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(runtime.status().contains("isn't running"));
}

#[test]
fn the_broker_exits_once_idle() {
    let repo = repo();
    let runtime = Runtime::new();
    let subscriber = Subscriber::spawn(&runtime, repo.path());
    subscriber.wait_for("ready", |l| l.starts_with("ready"));
    assert!(runtime.status().contains("running"));
    assert!(runtime.socket().exists());
    drop(subscriber);
    let start = Instant::now();
    while !runtime.status().contains("isn't running") {
        assert!(start.elapsed() < TIMEOUT, "the broker never exited");
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "idle exit {:?} after the last subscriber left (idle time 1s)",
        start.elapsed()
    );
    assert!(!runtime.socket().exists(), "the socket outlived the broker");
}

/// A change set kept current from a subscriber's lines, as the viewer does
/// from notices: changed paths refresh, git state or a rescan re-lists.
struct Mirror {
    set: ChangeSet,
    root: Root,
    rescans: usize,
    batches: usize,
    paths: usize,
    /// Every working-tree path reported, repository-relative.
    seen: BTreeSet<String>,
}

impl Mirror {
    fn new(dir: &Path) -> Self {
        let repo = Repo::discover(dir).expect("repo");
        let root = repo.watch_root().expect("root");
        let revisions = Revisions::default();
        let spec = repo.resolve_revisions(&revisions).expect("spec");
        let mut set = ChangeSet::new(repo, spec, revisions, &[], false);
        set.enumerate(|_| std::ops::ControlFlow::Continue(()))
            .expect("enumerate");
        Self {
            set,
            root,
            rescans: 0,
            batches: 0,
            paths: 0,
            seen: BTreeSet::new(),
        }
    }

    /// Whether `path` or a directory above it was reported.
    fn reported(&self, path: &str) -> bool {
        let mut prefix = path;
        loop {
            if self.seen.contains(prefix) {
                return true;
            }
            match prefix.rfind('/') {
                Some(i) => prefix = &prefix[..i],
                None => return false,
            }
        }
    }

    /// Applies the subscriber's lines until none arrive for `quiet`.
    fn settle(&mut self, subscriber: &Subscriber, quiet: Duration) {
        while let Ok(first) = subscriber.lines.recv_timeout(quiet) {
            let lines: Vec<String> = std::iter::once(first)
                .chain(subscriber.lines.try_iter())
                .map(|(_, line)| line)
                .collect();
            let mut worktree = Vec::new();
            let mut rescan = false;
            for line in &lines {
                if let Some(path) = line.strip_prefix("changed ") {
                    self.paths += 1;
                    match self.root.classify(Path::new(path)) {
                        Some(Change::Worktree(rel)) => {
                            self.seen.insert(rel.to_string());
                            worktree.push(rel);
                        }
                        Some(Change::Git) => rescan = true,
                        None => {}
                    }
                } else if line == "rescan" {
                    self.rescans += 1;
                    rescan = true;
                }
            }
            self.batches += 1;
            if rescan || worktree.len() > MAX_BATCH {
                self.set.rescan().expect("rescan");
            } else if !worktree.is_empty() {
                self.set.refresh(&worktree).expect("refresh");
            }
        }
    }

    fn listed(&self) -> BTreeSet<String> {
        self.set
            .files()
            .iter()
            .map(|c| c.path.to_string())
            .collect()
    }
}

/// Paths `git status` shows as changed in the working tree or untracked.
fn git_status(dir: &Path) -> BTreeSet<String> {
    git(dir, &["status", "--porcelain", "-uall", "--no-renames"])
        .lines()
        .filter(|line| line.as_bytes()[1] != b' ')
        .map(|line| line[3..].to_owned())
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn an_overflow_rescans_and_ends_where_git_does() {
    let repo = repo();
    let root = repo.path().canonicalize().expect("canonical");
    let runtime = Runtime::new();
    let subscriber = Subscriber::spawn(&runtime, &root);
    subscriber.wait_for("ready", |l| l.starts_with("ready"));
    let mut mirror = Mirror::new(&root);
    let queue: usize = std::fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .expect("read max_queued_events")
        .trim()
        .parse()
        .expect("a number");
    // Each new file with content queues a create, a modify and a close.
    let files = queue / 3 + 1_000;
    let pid = runtime.broker_pid();
    signal(&pid, "-STOP");
    let dir = root.join("flood");
    std::fs::create_dir(&dir).expect("mkdir");
    for i in 0..files {
        std::fs::write(root.join(format!("f{i}.txt")), "x").expect("write");
    }
    signal(&pid, "-CONT");
    subscriber.wait_for("the rescan", |l| l == "rescan");
    mirror.rescans += 1;
    mirror.set.rescan().expect("rescan");
    mirror.settle(&subscriber, Duration::from_millis(500));
    // The rebuilt watch set covers what was created during the overflow.
    std::fs::write(dir.join("after.txt"), "after").expect("write");
    mirror.settle(&subscriber, Duration::from_millis(500));
    let expected = git_status(&root);
    assert_eq!(expected.len(), files + 1);
    assert_eq!(mirror.listed(), expected);
    eprintln!(
        "queue of {queue} events, {files} files created while the broker was stopped; \
         {} rescans, {} batches after",
        mirror.rescans, mirror.batches
    );
}

/// A `git worktree` of the kernel clone in a temp dir, removed on drop.
struct Worktree {
    clone: PathBuf,
    path: PathBuf,
    _parent: tempfile::TempDir,
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let path = self.path.to_string_lossy().into_owned();
        Command::new("git")
            .arg("-C")
            .arg(&self.clone)
            .args(["worktree", "remove", "--force", &path])
            .status()
            .ok();
        Command::new("git")
            .arg("-C")
            .arg(&self.clone)
            .args(["worktree", "prune"])
            .status()
            .ok();
    }
}

#[test]
fn a_checkout_storm_ends_where_git_does() {
    let Some(clone) = std::env::var_os("DFFY_BENCH_REPO").map(PathBuf::from) else {
        eprintln!("DFFY_BENCH_REPO unset; skipping");
        return;
    };
    let parent = tempfile::tempdir().expect("tempdir");
    let path = parent.path().join("wt");
    let start = Instant::now();
    git(
        &clone,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &path.to_string_lossy(),
            "v6.0",
        ],
    );
    let worktree = Worktree {
        clone,
        path: path.canonicalize().expect("canonical"),
        _parent: parent,
    };
    eprintln!("worktree of v6.0 in {:?}", start.elapsed());
    let runtime = Runtime::new();
    let subscriber = Subscriber::spawn(&runtime, &worktree.path);
    let (_, ready) = subscriber.wait_for("ready", |l| l.starts_with("ready"));
    eprintln!("{ready}");
    let mut mirror = Mirror::new(&worktree.path);

    let checkout = Instant::now();
    let mut git_checkout = Command::new("git")
        .arg("-C")
        .arg(&worktree.path)
        .args(["checkout", "-q", "v6.1"])
        .spawn()
        .expect("checkout");
    // Applying notices while git runs, as a viewer would.
    while git_checkout.try_wait().expect("wait").is_none() {
        mirror.settle(&subscriber, Duration::from_millis(50));
    }
    let checked_out = checkout.elapsed();
    mirror.settle(&subscriber, Duration::from_secs(2));
    let settled = checkout.elapsed();
    let status = runtime.status();
    let expected = git_status(&worktree.path);
    let listed = mirror.listed();
    eprintln!(
        "checkout took {checked_out:?}, settled after {settled:?}: {} batches, {} paths, \
         {} rescans; git status lists {}, the mirror {}\n{status}",
        mirror.batches,
        mirror.paths,
        mirror.rescans,
        expected.len(),
        listed.len()
    );
    assert_eq!(listed, expected);
    // The index changing makes the mirror re-list everything anyway, which
    // would hide lost events; so check the events themselves: every file
    // the checkout touched was reported, or a directory above it was.
    let touched = git(
        &worktree.path,
        &["diff", "--name-only", "--no-renames", "v6.0", "v6.1"],
    );
    let touched: Vec<&str> = touched.lines().collect();
    let missed: Vec<_> = touched.iter().filter(|p| !mirror.reported(p)).collect();
    eprintln!(
        "{} files differ between v6.0 and v6.1; {} of them unreported{}",
        touched.len(),
        missed.len(),
        if mirror.rescans > 0 {
            " (after a rescan)"
        } else {
            ""
        }
    );
    assert!(
        mirror.rescans > 0 || missed.is_empty(),
        "unreported without a rescan: {:?}",
        &missed[..missed.len().min(10)]
    );
}

/// `fs.inotify.max_user_watches` as found; restored on drop.
#[cfg(target_os = "linux")]
struct WatchLimit(String);

#[cfg(target_os = "linux")]
impl WatchLimit {
    fn read() -> String {
        std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
            .expect("read the limit")
            .trim()
            .to_owned()
    }

    fn set(value: &str) {
        let status = Command::new("sudo")
            .args(["-n", "sysctl", "-q", "-w"])
            .arg(format!("fs.inotify.max_user_watches={value}"))
            .status()
            .expect("sudo");
        assert!(status.success(), "sudo sysctl");
    }
}

#[cfg(target_os = "linux")]
impl Drop for WatchLimit {
    fn drop(&mut self) {
        Self::set(&self.0);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn running_out_of_watches_falls_back_to_polling() {
    if std::env::var_os("DFFY_SYSCTL_TESTS").is_none() {
        eprintln!("DFFY_SYSCTL_TESTS unset; skipping");
        return;
    }
    let repo = repo();
    let root = repo.path().canonicalize().expect("canonical");
    let runtime = Runtime::new();
    let doctor = runtime
        .dffy(&["--color=never", "doctor"])
        .output()
        .expect("doctor");
    let doctor = String::from_utf8_lossy(&doctor.stdout).into_owned();
    let used: u64 = doctor
        .split("watch limit")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no watch count in {doctor}"));
    let original = WatchLimit::read();
    let limit = WatchLimit(original.clone());
    // Two watches can't cover the repository's directories plus .git and
    // refs.
    WatchLimit::set(&(used + 2).to_string());
    eprintln!(
        "max_user_watches {original} -> {} ({used} in use)",
        used + 2
    );
    let subscriber = Subscriber::spawn(&runtime, &root);
    let (_, polling) = subscriber.wait_for("polling", |l| l.starts_with("polling"));
    drop(limit);
    eprintln!(
        "{polling}; max_user_watches restored to {}",
        WatchLimit::read()
    );
    assert_eq!(WatchLimit::read(), original);
    let status = runtime.status();
    assert!(status.contains("polling"), "{status}");
    eprintln!("{status}");

    let mut mirror = Mirror::new(&root);
    subscriber.wait_for("the first poll's rescan", |l| l == "rescan");
    std::fs::write(root.join("src/lib.rs"), "fn f() { 1 }\n").expect("write");
    let written = Instant::now();
    let want = format!("changed {}", root.join("src/lib.rs").display());
    let (at, _) = subscriber.wait_for("the polled edit", |l| l == want);
    eprintln!("polling saw the edit after {:?}", at - written);
    mirror.set.refresh(&["src/lib.rs".into()]).expect("refresh");
    assert_eq!(mirror.listed(), git_status(&root));
}
