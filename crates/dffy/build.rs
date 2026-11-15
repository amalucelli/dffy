//! Names the build by a hash of the sources it's made from, so `dffy
//! doctor` can tell a running `dffyd` built from other sources, even of the
//! same version: the broker outlives rebuilds and reinstalls.

use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let inputs = [
        manifest.join("src"),
        manifest.join("Cargo.toml"),
        manifest.join("../dffy-core/src"),
        manifest.join("../dffy-core/Cargo.toml"),
        manifest.join("../../Cargo.lock"),
    ];
    let mut files = Vec::new();
    for input in &inputs {
        println!("cargo::rerun-if-changed={}", input.display());
        collect(input, &mut files);
    }
    files.sort();
    // FNV-1a: stable across Rust releases, unlike the std hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in &files {
        let rel = file.strip_prefix(&manifest).unwrap_or(file);
        let bytes = fs::read(file).unwrap_or_default();
        for byte in rel.as_os_str().as_encoded_bytes().iter().chain(&bytes) {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    println!("cargo::rustc-env=DFFY_SOURCE_HASH={:08x}", hash >> 32);
}

/// Every file at or below `path`; nothing when it's missing, as the
/// workspace's other crates are in a published package.
fn collect(path: &Path, files: &mut Vec<PathBuf>) {
    match fs::read_dir(path) {
        Ok(entries) => {
            for entry in entries.flatten() {
                collect(&entry.path(), files);
            }
        }
        Err(_) if path.is_file() => files.push(path.to_owned()),
        Err(_) => {}
    }
}
