//! Watching and listening through dffyd end to end. Subscribers are `dffy
//! daemon watch` processes, which go through the same client code as the
//! viewer: connect, start the broker if needed, fall back to watching
//! alone. Listeners are `dffy comments listen`, and the viewer handing over
//! to them a connection of the test's. Each test gets its own runtime
//! directory, so its own broker.
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
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
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

    fn control(&self) -> PathBuf {
        self.dir.path().join("dffy/control-v1.sock")
    }

    /// An older dffyd's control socket, for a test to answer on.
    fn older(&self) -> UnixListener {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(self.dir.path().join("dffy"))
            .expect("mkdir");
        UnixListener::bind(self.control()).expect("bind")
    }

    #[track_caller]
    fn wait_gone(&self) {
        let start = Instant::now();
        while !self.status().contains("isn't running") {
            assert!(start.elapsed() < TIMEOUT, "the broker never exited");
            std::thread::sleep(Duration::from_millis(100));
        }
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
    runtime.wait_gone();
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
        // Without the user's plugins: an installed dffy-agent probes agents.
        .env("PATH", "/usr/bin:/bin")
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

/// A `dffy comments listen` process, and its lines on stdout and stderr.
struct Listener {
    child: Child,
    lines: mpsc::Receiver<String>,
    errors: mpsc::Receiver<String>,
}

fn lines_of(from: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(from).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    lines
}

impl Listener {
    fn spawn(runtime: &Runtime, dir: &Path) -> Self {
        let mut child = runtime
            .dffy(&["comments", "listen", "--as", "claude", "--key", "7"])
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn listener");
        let lines = lines_of(child.stdout.take().expect("stdout"));
        let errors = lines_of(child.stderr.take().expect("stderr"));
        Self {
            child,
            lines,
            errors,
        }
    }

    #[track_caller]
    fn line(&self) -> String {
        self.lines.recv_timeout(TIMEOUT).expect("a line")
    }

    #[track_caller]
    fn wait_exit(&mut self) {
        let start = Instant::now();
        while self.child.try_wait().expect("wait").is_none() {
            assert!(start.elapsed() < TIMEOUT, "it never ended");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// A connection to the control API.
struct Raw {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Raw {
    /// Connected once dffyd is there.
    #[track_caller]
    fn connect(runtime: &Runtime) -> Self {
        let start = Instant::now();
        let stream = loop {
            match UnixStream::connect(runtime.control()) {
                Ok(stream) => break stream,
                Err(e) => assert!(start.elapsed() < TIMEOUT, "no dffyd: {e}"),
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut raw = Self::on(stream);
        raw.send(r#"{"version":1}"#);
        assert!(raw.recv().contains(r#""capabilities""#));
        raw
    }

    fn on(stream: UnixStream) -> Self {
        stream.set_read_timeout(Some(TIMEOUT)).expect("timeout");
        Self {
            writer: stream.try_clone().expect("clone"),
            reader: BufReader::new(stream),
        }
    }

    /// Registered as a viewer of `worktree`.
    #[track_caller]
    fn view(runtime: &Runtime, worktree: &Path) -> Self {
        let worktree = worktree.canonicalize().expect("canonical");
        let mut raw = Self::connect(runtime);
        raw.send(&format!(
            r#"{{"id":1,"method":"views/register","params":{{"pid":1,"worktree":"{}","repository":"","diff":""}}}}"#,
            worktree.display()
        ));
        assert!(raw.recv().contains(r#""result""#));
        raw
    }

    fn send(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .expect("send");
    }

    fn recv(&mut self) -> String {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("a line");
        line
    }

    /// The id of the listener handed to this view next.
    #[track_caller]
    fn handed(&mut self) -> String {
        let message = self.recv();
        assert!(
            message.contains(r#""method":"comments/listen""#),
            "{message}"
        );
        assert!(message.contains(r#""client":"claude""#), "{message}");
        id_of(&message)
    }

    fn hand_over(&mut self, id: &str, prompt: &str) {
        self.send(&format!(
            r#"{{"id":{id},"result":{{"prompt":"{prompt}"}}}}"#
        ));
    }
}

/// The request id of `message`, which opens with it.
fn id_of(message: &str) -> String {
    message
        .strip_prefix(r#"{"id":"#)
        .and_then(|rest| rest.split_once(','))
        .map_or_else(|| panic!("no id in {message}"), |(id, _)| id.to_owned())
}

/// A listener keeps the dffyd it started, as a viewer does.
#[test]
fn a_listener_keeps_the_broker_until_it_leaves() {
    let repo = repo();
    let runtime = Runtime::new();
    let listener = Listener::spawn(&runtime, repo.path());
    let mut view = Raw::view(&runtime, repo.path());
    view.handed();
    drop(view);
    let pid = runtime.broker_pid();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(runtime.broker_pid(), pid, "not started again");
    drop(listener);
    runtime.wait_gone();
}

/// An agent ends a session by killing its processes' descendants, and the
/// listener's dffyd serves everyone: it's nobody's descendant.
#[test]
fn the_broker_is_not_its_starter_s_descendant() {
    let repo = repo();
    let runtime = Runtime::new();
    let listener = Listener::spawn(&runtime, repo.path());
    Raw::view(&runtime, repo.path()).handed();
    let starter = listener.child.id().to_string();
    let broker = runtime.broker_pid();
    let start = Instant::now();
    loop {
        let line = ancestors(&broker);
        if !line.contains(&starter) {
            break;
        }
        assert!(start.elapsed() < TIMEOUT, "{starter} among {line:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The pids above `pid`, its parent first.
fn ancestors(pid: &str) -> Vec<String> {
    let parent = |pid: &String| {
        let out = Command::new("ps")
            .args(["-o", "ppid=", "-p", pid])
            .output()
            .expect("ps");
        let parent = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (!parent.is_empty() && parent != "0").then_some(parent)
    };
    std::iter::successors(parent(&pid.to_owned()), parent).collect()
}

/// From a directory of the working tree; each hand-over is a line of its
/// own, whatever the prompt holds, and the listener is there for the next.
#[test]
fn a_listener_prints_each_prompt_as_a_line() {
    let repo = repo();
    let runtime = Runtime::new();
    let listener = Listener::spawn(&runtime, &repo.path().join("src"));
    let mut view = Raw::view(&runtime, repo.path());
    let id = view.handed();
    view.hand_over(&id, "thread 1\\nand\\u001b[2J more\\ttoo");
    assert_eq!(listener.line(), "thread 1^Jand^[[2J more    too");
    let id = view.handed();
    view.hand_over(&id, "thread 2");
    assert_eq!(listener.line(), "thread 2");
}

#[test]
fn a_listener_starts_dffyd_again_when_it_goes() {
    let repo = repo();
    let runtime = Runtime::new();
    let listener = Listener::spawn(&runtime, repo.path());
    let mut view = Raw::view(&runtime, repo.path());
    view.handed();
    let stopped = runtime.dffy(&["daemon", "stop"]).output().expect("stop");
    assert!(stopped.status.success());
    while !view.recv().is_empty() {}
    let mut view = Raw::view(&runtime, repo.path());
    let id = view.handed();
    view.hand_over(&id, "thread 1");
    assert_eq!(listener.line(), "thread 1");
}

/// It says so once, stays connected, and listens once the dffyd that
/// doesn't know how is gone.
#[test]
fn a_listener_says_once_that_dffyd_is_older() {
    let repo = repo();
    let runtime = Runtime::new();
    let older = runtime.older();
    let mut listener = Listener::spawn(&runtime, repo.path());
    let mut said = Vec::new();
    for _ in 0..2 {
        let mut raw = Raw::on(older.accept().expect("accept").0);
        raw.recv();
        raw.send(r#"{"version":1,"capabilities":{"methods":[],"events":[]}}"#);
        let id = id_of(&raw.recv());
        raw.send(&format!(
            r#"{{"id":{id},"error":{{"code":"unknown_method","message":"no method"}}}}"#
        ));
        said.extend(listener.errors.recv_timeout(Duration::from_millis(500)));
        assert!(listener.child.try_wait().expect("wait").is_none());
    }
    drop(older);
    let mut view = Raw::view(&runtime, repo.path());
    view.handed();
    said.extend(listener.errors.try_iter());
    said.retain(|line| line.contains("dffy daemon stop"));
    assert_eq!(said.len(), 1, "{said:?}");
}

/// Nothing is there to listen for, and no dffyd is started to wait on.
#[test]
fn a_listener_outside_a_repository_stays_idle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = Runtime::new();
    let mut listener = Listener::spawn(&runtime, dir.path());
    std::thread::sleep(Duration::from_secs(1));
    assert!(listener.child.try_wait().expect("wait").is_none());
    assert!(runtime.status().contains("isn't running"));
    assert_eq!(listener.lines.try_recv().ok(), None);
    assert_eq!(listener.errors.try_recv().ok(), None);
}

/// No delivery is needed for it to notice.
#[test]
fn a_listener_ends_when_nobody_reads_it() {
    let repo = repo();
    let runtime = Runtime::new();
    let mut child = runtime
        .dffy(&["comments", "listen", "--as", "claude"])
        .current_dir(repo.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn listener");
    let stdout = child.stdout.take();
    let (_lines, lines) = mpsc::channel();
    let (_errors, errors) = mpsc::channel();
    let mut listener = Listener {
        child,
        lines,
        errors,
    };
    let mut view = Raw::view(&runtime, repo.path());
    view.handed();
    drop(stdout);
    listener.wait_exit();
    assert!(view.recv().contains("$/cancelRequest"));
}

/// A session killed hard leaves no ghost, even one whose listener writes
/// to a file, which has no reader to miss.
#[test]
fn a_listener_ends_with_its_parent() {
    let repo = repo();
    let runtime = Runtime::new();
    let stub = runtime.dir.path().join("listener-parent.sh");
    std::fs::write(&stub, include_str!("fixtures/stubs/listener-parent.sh")).expect("write");
    let mut dffy = runtime.dffy(&[]);
    let mut parent = Command::new("sh");
    for (key, value) in dffy.current_dir(repo.path()).get_envs() {
        parent.env(key, value.expect("set"));
    }
    let mut parent = parent
        .arg(&stub)
        .arg(env!("CARGO_BIN_EXE_dffy"))
        .current_dir(repo.path())
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn parent");
    let mut view = Raw::view(&runtime, repo.path());
    view.handed();
    parent.kill().expect("kill");
    parent.wait().expect("wait");
    assert!(view.recv().contains("$/cancelRequest"));
}

#[test]
fn a_listener_dies_on_each_signal_to_end() {
    let repo = repo();
    let runtime = Runtime::new();
    for name in ["-TERM", "-INT", "-HUP"] {
        let mut listener = Listener::spawn(&runtime, repo.path());
        let mut view = Raw::view(&runtime, repo.path());
        view.handed();
        signal(&listener.child.id().to_string(), name);
        listener.wait_exit();
    }
}

/// Hooks run it on every edit, so it never fails, says nothing, a config's
/// problems included, and starts nothing, with no dffyd, an older one, or
/// one that knows the listener.
#[test]
fn listening_reports_activity_and_never_fails() {
    let repo = repo();
    let report = |runtime: &Runtime| {
        let config = runtime.dir.path().join("config/dffy");
        std::fs::create_dir_all(&config).expect("mkdir");
        std::fs::write(config.join("config"), "no-such-key = 1\ntheme = no-such\n").expect("write");
        let out = runtime
            .dffy(&["comments", "listening", "--key", "7", "--edited"])
            .current_dir("/")
            .output()
            .expect("listening");
        assert!(out.status.success());
        assert_eq!((out.stdout, out.stderr), (Vec::new(), Vec::new()));
    };
    let runtime = Runtime::new();
    report(&runtime);
    assert!(runtime.status().contains("isn't running"));

    let _listener = Listener::spawn(&runtime, repo.path());
    let mut view = Raw::view(&runtime, repo.path());
    let id = view.handed();
    report(&runtime);
    let changed = view.recv();
    let edited = format!(r#"{{"method":"listener/changed","params":{{"id":{id},"#);
    assert!(changed.starts_with(&edited), "{changed}");
    assert!(changed.contains(r#""prompted":null"#), "{changed}");

    let runtime = Runtime::new();
    let older = runtime.older();
    let answering = std::thread::spawn(move || {
        let mut raw = Raw::on(older.accept().expect("accept").0);
        raw.recv();
        raw.send(r#"{"version":1,"capabilities":{"methods":[],"events":[]}}"#);
        raw.send(r#"{"id":1,"error":{"code":"unknown_method","message":"no method"}}"#);
        raw.recv()
    });
    report(&runtime);
    assert!(
        answering
            .join()
            .expect("asked")
            .contains("comments/listening")
    );
}
