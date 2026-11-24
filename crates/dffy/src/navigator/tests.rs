//! The navigator protocol's host side against its contract: every value
//! goes through JSON and back unchanged, and the host writes the fixture's
//! params and reads its answers and pushes, which the helper's own tests
//! hold it to.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use dffy_core::cache::Cache;
use rustix::process::{Pid, Signal, kill_process};

use dffy_core::git::{Mode, ObjectId, PathRoot, Side as GitSide, Snapshot};
use dffy_core::highlight::DefinitionKind;
use dffy_core::navigate::{Candidate, Filter, Progress, Rank, Reference, Use};
use dffy_core::view::Side;

use super::{PROTOCOL, client, wire};
use crate::plugin::json::{Value, parse};

const FIXTURE: &str = include_str!("../../tests/fixtures/navigate-protocol/messages.json");

fn id(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.as_bytes()).expect("hex")
}

/// Every kind's name, as a tags query captures them.
const KINDS: &[&str] = &[
    "function",
    "method",
    "macro",
    "type",
    "struct",
    "union",
    "enum",
    "enumerator",
    "trait",
    "constant",
    "static",
    "variable",
    "module",
    "class",
    "interface",
    "field",
    "reexport",
    "local",
    "parameter",
    "import",
];

fn round_trip<T: PartialEq + std::fmt::Debug + Clone>(
    value: &T,
    write: impl Fn(&T, &mut String),
    read: impl Fn(&Value) -> Result<T, String>,
) {
    let mut s = String::new();
    write(value, &mut s);
    assert_eq!(
        read(&parse(s.as_bytes()).expect("json")),
        Ok(value.clone()),
        "{s}"
    );
}

/// Each value the protocol carries reads back as it was written, in every
/// kind it can take.
#[test]
fn values_round_trip_through_json() {
    let blob = |mode| GitSide::Object {
        id: id("4b825dc642cb6eb9a060e54bf8d69288fbee4904"),
        mode,
    };
    let mut files: Vec<GitSide> = [Mode::File, Mode::Executable, Mode::Symlink, Mode::Gitlink]
        .into_iter()
        .map(blob)
        .collect();
    files.extend([
        GitSide::Worktree {
            mode: Mode::File,
            id: None,
        },
        GitSide::Worktree {
            mode: Mode::Executable,
            id: Some(id("4b825dc642cb6eb9a060e54bf8d69288fbee4904")),
        },
        GitSide::Path {
            mode: Mode::File,
            root: PathRoot::Old,
        },
        GitSide::Path {
            mode: Mode::Symlink,
            root: PathRoot::New,
        },
    ]);
    let kinds = (KINDS.iter())
        .map(|name| Some(DefinitionKind::from_name(name).expect("a kind")))
        .chain([None]);
    let ranks = [Rank::SameFile, Rank::Imported, Rank::Near, Rank::Elsewhere];
    for (i, kind) in kinds.enumerate() {
        let c = Candidate {
            path: "include/linux/slab.h".into(),
            side: files[i % files.len()],
            kind,
            start: 611,
            end: 620,
            container: kind.map(|_| "Vec".to_owned()),
            rank: ranks[i % ranks.len()],
            changed: i % 2 == 0,
            in_hunk: [None, Some(false), Some(true)][i % 3],
        };
        round_trip(&c, wire::candidate, wire::read_candidate);
        let r = Reference {
            candidate: c,
            text: "kmalloc_array(n, \"size\")".to_owned(),
            more: i,
        };
        round_trip(&r, wire::reference, wire::read_reference);
    }
    for at in [
        Snapshot::Tree(id("4b825dc642cb6eb9a060e54bf8d69288fbee4904")),
        Snapshot::Index,
        Snapshot::Worktree,
    ] {
        round_trip(&at, |at, s| wire::snapshot(*at, s), wire::read_snapshot);
    }
    let p = Progress {
        done: 12,
        total: 55_191,
    };
    round_trip(&p, |p, s| wire::progress(*p, s), wire::read_progress);
    for side in [Side::Old, Side::New] {
        let s = format!(r#"{{"side":"{}"}}"#, wire::side(side));
        assert_eq!(
            wire::read_side(&parse(s.as_bytes()).expect("json"), "side"),
            Ok(side)
        );
    }
    for w in [
        dffy_core::diff::Whitespace::Exact,
        dffy_core::diff::Whitespace::IgnoreAll,
        dffy_core::diff::Whitespace::IgnoreChange,
    ] {
        let s = format!(r#"{{"w":"{}"}}"#, wire::whitespace(w));
        assert_eq!(
            wire::read_whitespace(&parse(s.as_bytes()).expect("json"), "w"),
            Ok(w)
        );
    }
}

fn fixture() -> Value {
    parse(FIXTURE.as_bytes()).expect("the fixture is JSON")
}

fn message<'a>(fixture: &'a Value, name: &str) -> &'a Value {
    fixture
        .get(name)
        .unwrap_or_else(|| panic!("the fixture has no {name}"))
}

fn params(fixture: &Value, name: &str) -> Value {
    message(message(fixture, name), "params").clone()
}

fn json(text: &str) -> Value {
    parse(text.as_bytes()).expect("json")
}

/// The host writes the fixture's params, and reads its results and the
/// progress the helper pushes.
#[test]
fn the_host_side_speaks_the_fixture() {
    let f = fixture();
    let init = format!(
        r#"{{"protocol":{PROTOCOL},"client":{{"name":"dffy","version":"0.0.0"}}{}}}"#,
        client::initialize_members(1, std::path::Path::new("@CACHE@"), 16_777_216)
    );
    assert_eq!(json(&init), params(&f, "initialize"));

    let snapshots = params(&f, "open");
    let tree = (snapshots.get("snapshots").and_then(Value::as_array)).expect("two")[0].clone();
    let open = client::open_params(
        [
            wire::read_snapshot(&tree).expect("snapshot"),
            Snapshot::Worktree,
        ],
        Side::New,
        dffy_core::diff::Whitespace::Exact,
    );
    assert_eq!(json(&open), params(&f, "open"));
    let opened = client::read_opened(message(message(&f, "open"), "result")).expect("reads");
    assert_eq!(opened.languages, ["c"]);

    let pushed = params(&f, "pushed");
    let built = client::read_progress(&pushed).expect("reads");
    assert!(built.progress.complete() && !built.building);
    let built = client::read_progress(message(message(&f, "progress"), "result")).expect("reads");
    assert!(built.progress.complete());

    let origin = (b"src/use.c".as_slice(), "c");
    let lookup = client::asking_params(opened.navigator, "frob", origin, "");
    assert_eq!(json(&lookup), params(&f, "lookup"));
    let found = client::read_lookup(message(message(&f, "lookup"), "result")).expect("reads");
    assert_eq!(found.candidates.len(), 1);
    let at = Use {
        line: 5,
        column: Some(8),
    };
    let mut place = String::new();
    wire::at(Some(at), &mut place);
    let lookup_at = client::asking_params(opened.navigator, "frob", origin, &place);
    assert_eq!(json(&lookup_at), params(&f, "lookupAt"));
    assert_eq!(wire::read_use(&params(&f, "lookupAt")), Ok(Some(at)));
    let line = Use { column: None, ..at };
    let mut place = String::new();
    wire::at(Some(line), &mut place);
    let lookup_line = json(&client::asking_params(
        opened.navigator,
        "frob",
        origin,
        &place,
    ));
    assert_eq!(wire::read_use(&lookup_line), Ok(Some(line)));
    assert_eq!(wire::read_use(&params(&f, "lookup")), Ok(None));
    let filter = Filter {
        calls_only: true,
        in_diff: true,
    };
    let mut extra = String::new();
    wire::filter(filter, &mut extra);
    assert_eq!(
        wire::read_filter(&json(&format!(r#"{{"x":1{extra}}}"#))),
        Ok(filter)
    );
    assert_eq!(
        wire::read_filter(&params(&f, "referencesStart")),
        Ok(Filter::default())
    );

    let extra = client::search_extra("references", 0);
    let start = client::asking_params(opened.navigator, "frob", origin, &extra);
    assert_eq!(json(&start), params(&f, "referencesStart"));
    let extra = client::search_extra("text", 50);
    let start = client::asking_params(opened.navigator, "zap", origin, &extra);
    assert_eq!(json(&start), params(&f, "textStart"));

    let second = Duration::from_secs(1);
    let step = client::step_params(1, second, None, true);
    assert_eq!(json(&step), params(&f, "referencesStep"));
    let step = client::step_params(2, second, Some(50), true);
    assert_eq!(json(&step), params(&f, "textStep"));
    let result = |name| message(message(&f, name), "result").clone();
    client::read_step(&result("referencesStep"), wire::read_reference).expect("reads");
    client::read_step(&result("textStep"), wire::read_candidate).expect("reads");
}

/// A stopped helper's process, killed when dropped, a failed test's
/// included: stopped, it would never exit on its own.
struct Stopped(Pid);

impl Drop for Stopped {
    fn drop(&mut self) {
        kill_process(self.0, Signal::KILL).ok();
    }
}

fn stopped_helper(root: &std::path::Path, cache: &Cache) -> (client::Helper, Stopped) {
    client::use_program(super::built_program());
    let helper = client::Helper::start(root, cache, 1, None).expect("starts");
    // Answered once, so it's past `initialize` when it's stopped.
    helper.request("progress", PROGRESS.to_owned(), None).ok();
    let pid = helper.pid().expect("runs");
    kill_process(pid, Signal::STOP).expect("stops");
    (helper, Stopped(pid))
}

const PROGRESS: &str = r#"{"navigator":0}"#;

/// Asks `helper` on a thread of its own, with `cancel`.
fn asking(
    helper: &client::Helper,
    cancel: &Arc<client::Cancel>,
) -> mpsc::Receiver<client::Refused> {
    let (tx, rx) = mpsc::channel();
    let (helper, cancel) = (helper.clone(), Arc::clone(cancel));
    std::thread::spawn(move || {
        let answer = helper.request("progress", PROGRESS.to_owned(), Some(&cancel));
        tx.send(answer.expect_err("a stopped helper answers nothing"))
            .ok();
    });
    // Sent before what the test does next.
    std::thread::sleep(Duration::from_millis(100));
    rx
}

/// A request waiting on a helper that doesn't answer ends when it's
/// cancelled, after the wait for the helper's own answer, and when the
/// helper dies, cancel or not.
#[test]
fn a_request_in_flight_ends_when_cancelled_or_its_helper_dies() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(dir.path())
        .status()
        .expect("git init");
    let cache = Cache::new(dir.path().join("cache"), 1 << 20);
    let bound = Duration::from_secs(10);

    let (helper, stopped) = stopped_helper(dir.path(), &cache);
    let cancel = Arc::new(client::Cancel::default());
    let answer = asking(&helper, &cancel);
    let start = Instant::now();
    cancel.cancel();
    let refused = answer.recv_timeout(bound).expect("ends once cancelled");
    assert_eq!(refused.message, "cancelled");
    assert!(start.elapsed() < bound);
    drop(stopped);
    helper.close();

    let (helper, stopped) = stopped_helper(dir.path(), &cache);
    let cancel = Arc::new(client::Cancel::default());
    let answer = asking(&helper, &cancel);
    drop(stopped);
    let refused = answer
        .recv_timeout(bound)
        .expect("ends once the helper is gone");
    assert!(refused.helper_gone(), "{refused:?}");
}

/// The profile's folder is found wherever cargo puts test binaries in it
/// and whatever else it keeps there, the folder of `dffy`'s own builds
/// not taken for the binary.
#[test]
fn the_profile_of_a_test_binary_holds_dffy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("target/debug");
    let nested = profile.join("build/dffy/1f/out");
    std::fs::create_dir_all(&nested).expect("mkdir");
    std::fs::write(profile.join("dffy"), "").expect("write");
    for exe in [nested.join("dffy-1f"), profile.join("deps/dffy-1f")] {
        assert_eq!(super::profile(&exe), Some(profile.as_path()));
    }
}

/// After a helper fails, the first host thread to ask starts another; one
/// still holding the failed one's generation gets the new one, not a third.
#[test]
fn a_failed_helper_is_replaced_once() {
    client::use_program(super::built_program());
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = Cache::new(dir.path().join("cache"), 1 << 20);
    let start = |_| client::Helper::start(dir.path(), &cache, 1, None);
    let mut helpers = client::Helpers::default();
    let (first, _) = helpers.get(None, start).expect("starts");
    let gone = Some((first, "gone"));
    let (second, _) = helpers.get(gone, start).expect("starts another");
    let (again, _) = helpers.get(gone, start).expect("the one running");
    assert_ne!(second, first);
    assert_eq!(again, second, "replaced once");
    helpers.close();
}

/// Past the restart limit every ask says why the last helper failed, one
/// that comes with no failure of its own too.
#[test]
fn past_the_restart_limit_the_last_failure_is_said() {
    client::use_program(super::built_program());
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = Cache::new(dir.path().join("cache"), 1 << 20);
    let start = |_| client::Helper::start(dir.path(), &cache, 1, None);
    let mut helpers = client::Helpers::default();
    let mut gone = None;
    let refused = loop {
        match helpers.get(gone, start) {
            Ok((generation, _)) => gone = Some((generation, "out of luck")),
            Err(why) => break why,
        }
    };
    assert!(refused.ends_with("keeps exiting: out of luck"), "{refused}");
    let later = helpers.get(None, start).err();
    assert_eq!(later, Some(refused));
}
