//! Time from spawning `dffy v6.0..v6.1` in a 200×60 pseudo-terminal to its
//! first complete frame, on the kernel clone named by `DFFY_BENCH_REPO`.
//! Time a release build:
//!
//! ```sh
//! cargo build --release && cargo run --release --example first_frame -- target/release/dffy
//! ```
//!
//! A second argument names a config directory to run with instead of
//! none, such as one with a `render` setting.
#![expect(
    clippy::expect_used,
    reason = "a measuring tool: a failed step ends the run with its message"
)]

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const RUNS: usize = 15;
/// The end of a synchronized update closes every frame dffy draws.
const FRAME_END: &[u8] = b"\x1b[?2026l";

fn main() {
    let Some(repo) = std::env::var_os("DFFY_BENCH_REPO") else {
        eprintln!("DFFY_BENCH_REPO unset, skipping");
        return;
    };
    let bin = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/release/dffy".to_owned());
    // The child starts in the kernel clone, so a relative path won't do.
    let bin = std::fs::canonicalize(&bin).expect("the dffy binary exists");
    let config = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "/nonexistent".to_owned());
    let mut first_byte = Vec::new();
    let mut first_frame = Vec::new();
    for _ in 0..RUNS {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 60,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open a pty");
        let mut cmd = CommandBuilder::new(&bin);
        cmd.arg("v6.0..v6.1");
        cmd.cwd(&repo);
        cmd.env("TERM", "xterm-256color");
        cmd.env("XDG_CONFIG_HOME", &config);
        cmd.env(
            "XDG_CACHE_HOME",
            std::env::temp_dir().join("dffy-first-frame"),
        );
        // Not the user's own: their remembered choices would skew the
        // timing, and the run would overwrite them.
        cmd.env(
            "XDG_STATE_HOME",
            std::env::temp_dir().join("dffy-first-frame-state"),
        );
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let mut writer = pair.master.take_writer().expect("writer");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send((Instant::now(), buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        });
        let start = Instant::now();
        let mut child = pair.slave.spawn_command(cmd).expect("spawn dffy");
        drop(pair.slave);
        let mut seen = Vec::new();
        let mut byte_at = None;
        loop {
            let (at, bytes) = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("dffy draws within 10s");
            byte_at.get_or_insert(at);
            seen.extend_from_slice(&bytes);
            if seen.windows(FRAME_END.len()).any(|w| w == FRAME_END) {
                first_frame.push(at - start);
                break;
            }
        }
        first_byte.push(byte_at.expect("got bytes") - start);
        writer.write_all(b"q").expect("quit");
        child.wait().expect("dffy exits");
    }
    for (name, mut times) in [("first byte", first_byte), ("first frame", first_frame)] {
        times.sort();
        println!(
            "{name}: median {:.1}ms, min {:.1}ms, max {:.1}ms over {RUNS} runs",
            ms(times[RUNS / 2]),
            ms(times[0]),
            ms(times[RUNS - 1])
        );
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
