use std::process::Command;

use super::*;

fn cache(budget: u64) -> (tempfile::TempDir, Cache) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = Cache::new(dir.path().join("dffy"), budget);
    (dir, cache)
}

#[test]
fn entries_round_trip_by_kind_and_key() {
    let (_dir, cache) = cache(1 << 20);
    assert_eq!(cache.get(Kind::Renames, b"k"), None);
    cache.put(Kind::Renames, b"k", b"value");
    assert_eq!(
        cache.get(Kind::Renames, b"k").as_deref(),
        Some(&b"value"[..])
    );
    assert_eq!(cache.get(Kind::Highlight, b"k"), None);
    assert_eq!(cache.get(Kind::Renames, b"other"), None);
    cache.put(Kind::Renames, b"empty", b"");
    assert_eq!(
        cache.get(Kind::Renames, b"empty").as_deref(),
        Some(&b""[..])
    );
}

#[test]
fn corrupted_entries_are_misses_and_get_removed() {
    let (_dir, cache) = cache(1 << 20);
    let value = vec![7u8; 1000];
    for damage in [
        |b: &mut Vec<u8>| b.truncate(b.len() - 1),
        |b: &mut Vec<u8>| b[HEADER + 10] ^= 1,
        |b: &mut Vec<u8>| b[0] = b'x',
        |b: &mut Vec<u8>| b.truncate(3),
    ] {
        cache.put(Kind::Highlight, b"key", &value);
        let path = cache.path(Kind::Highlight, b"key");
        let mut bytes = fs::read(&path).expect("entry written");
        damage(&mut bytes);
        fs::write(&path, &bytes).expect("rewrite");
        assert_eq!(cache.get(Kind::Highlight, b"key"), None);
        assert!(!path.exists(), "corrupt entry removed");
    }
}

#[test]
fn eviction_drops_the_least_recently_used_down_to_the_target() {
    let (_dir, cache) = cache(10_000);
    let value = vec![1u8; 1000 - HEADER];
    let old = SystemTime::now() - Duration::from_secs(3600);
    for i in 0..15u8 {
        let path = cache.path(Kind::Renames, &[i]);
        write_atomic(&path, Kind::Renames, &value).expect("write");
        let age = old + Duration::from_secs(u64::from(i));
        fs::File::options()
            .append(true)
            .open(&path)
            .and_then(|f| f.set_modified(age))
            .expect("set mtime");
    }
    // A hit makes entry 0 the most recently used.
    assert!(cache.get(Kind::Renames, &[0]).is_some());
    assert_eq!(cache.usage().expect("usage"), (15_000, 15));
    cache.evict().expect("evict");
    assert_eq!(cache.usage().expect("usage"), (8000, 8));
    assert!(cache.get(Kind::Renames, &[0]).is_some());
    for i in 1..8u8 {
        assert!(
            cache.get(Kind::Renames, &[i]).is_none(),
            "entry {i} evicted"
        );
    }
}

const CHILD_DIR: &str = "DFFY_CACHE_TEST_WRITER_DIR";
const CHILD_SEED: &str = "DFFY_CACHE_TEST_WRITER_SEED";
const ROUNDS: usize = 200;

/// A value that checks itself: every byte is the seed.
fn value(seed: u8) -> Vec<u8> {
    vec![seed; 64 << 10]
}

/// Not a test of its own: the body of the writer processes spawned by
/// `concurrent_processes_never_expose_a_partial_entry`.
#[test]
fn writer_child() {
    let (Some(dir), Some(seed)) = (std::env::var_os(CHILD_DIR), std::env::var(CHILD_SEED).ok())
    else {
        return;
    };
    let cache = Cache::new(dir.into(), 1 << 30);
    let seed: u8 = seed.parse().expect("seed");
    for _ in 0..ROUNDS {
        cache.put(Kind::Highlight, b"shared", &value(seed));
    }
}

#[test]
fn concurrent_processes_never_expose_a_partial_entry() {
    let (_dir, cache) = cache(1 << 30);
    let exe = std::env::current_exe().expect("test binary");
    let mut children: Vec<_> = [1u8, 2]
        .iter()
        .map(|seed| {
            Command::new(&exe)
                .args(["--exact", "cache::tests::writer_child", "--quiet"])
                .env(CHILD_DIR, cache.dir())
                .env(CHILD_SEED, seed.to_string())
                .spawn()
                .expect("spawn writer")
        })
        .collect();
    let mut seen = 0;
    while children
        .iter_mut()
        .any(|c| c.try_wait().expect("wait").is_none())
    {
        if let Some(bytes) = cache.get(Kind::Highlight, b"shared") {
            assert!(bytes == value(1) || bytes == value(2), "torn entry");
            seen += 1;
        }
    }
    for mut child in children {
        assert!(child.wait().expect("wait").success());
    }
    let last = cache
        .get(Kind::Highlight, b"shared")
        .expect("an entry survives");
    assert!(last == value(1) || last == value(2));
    eprintln!("read {seen} intact entries while the writers ran");
    let temporaries = cache
        .entries()
        .expect("entries")
        .iter()
        .filter(|e| is_temporary(&e.0))
        .count();
    assert_eq!(temporaries, 0);
}

#[test]
fn varints_round_trip() {
    let mut out = Vec::new();
    let values = [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX];
    for &v in &values {
        put_varint(&mut out, v);
    }
    let mut input = &out[..];
    for &v in &values {
        assert_eq!(get_varint(&mut input), Some(v));
    }
    assert_eq!(get_varint(&mut input), None);
}
