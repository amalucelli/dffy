//! `cargo xtask bump <version>`: sets the release version everywhere it's
//! written, so `task release` finds the tag matching Cargo. That's the
//! workspace version (dffy, the plugins, xtask), dffy-core's own version,
//! and the version requirement of every path dependency between workspace
//! crates. `Cargo.lock` is refreshed for the workspace crates only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{ROOT, Result, output};

pub(crate) fn main(args: &[String]) -> Result<()> {
    let [version] = args else {
        return Err("usage: cargo xtask bump <version>, e.g. 0.3.0".to_owned());
    };
    let version = version.strip_prefix('v').unwrap_or(version);
    if !is_version(version) {
        return Err(format!(
            "{version}: not a version of the form MAJOR.MINOR.PATCH"
        ));
    }
    let root = Path::new(ROOT);
    let mut changed = Vec::new();
    for path in manifests(root)? {
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let bumped = bump(&text, version);
        if bumped != text {
            fs::write(&path, bumped).map_err(|e| format!("{}: {e}", path.display()))?;
            changed.push(
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string(),
            );
        }
    }
    output(
        Command::new("cargo")
            .args(["update", "--workspace", "--offline"])
            .current_dir(root),
    )?;
    if changed.is_empty() {
        println!("already at {version}");
    } else {
        println!("{version} in {} and Cargo.lock", changed.join(", "));
    }
    println!("then commit, and run task release TAG=v{version}");
    Ok(())
}

fn is_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The root manifest and every member's.
fn manifests(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = vec![root.join("Cargo.toml")];
    for dir in ["crates", "plugins"] {
        let dir = root.join(dir);
        let entries = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let manifest = entry.map_err(|e| e.to_string())?.path().join("Cargo.toml");
            if manifest.is_file() {
                paths.push(manifest);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// `text` with `version` in the version line of `[package]` and
/// `[workspace.package]`, and in every path dependency's requirement. Other
/// lines, including dependencies from crates.io, stay as they are.
fn bump(text: &str, version: &str) -> String {
    let mut section = "";
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            section = trimmed.trim_end();
        }
        let package_version = matches!(section, "[package]" | "[workspace.package]")
            && trimmed.starts_with("version = \"");
        let path_dependency = trimmed.contains("path = \"") && trimmed.contains("version = \"");
        let replaced = if package_version || path_dependency {
            replace_version(line, version)
        } else {
            None
        };
        out.push_str(replaced.as_deref().unwrap_or(line));
    }
    out
}

/// `line` with the string after its first `version = "` replaced.
fn replace_version(line: &str, version: &str) -> Option<String> {
    let start = line.find("version = \"")? + "version = \"".len();
    let end = start + line[start..].find('"')?;
    Some(format!("{}{version}{}", &line[..start], &line[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bumps_package_versions_and_path_requirements_only() {
        let manifest = r#"[workspace.package]
version = "0.2.0"
edition = "2024"

[package]
name = "dffy"
version = "0.2.0"

[dependencies]
dffy-core = { version = "0.2.0", path = "../dffy-core" }
gix = { version = "0.90.0", default-features = false }
memchr = "2.8.3"
"#;
        let want = r#"[workspace.package]
version = "0.3.0"
edition = "2024"

[package]
name = "dffy"
version = "0.3.0"

[dependencies]
dffy-core = { version = "0.3.0", path = "../dffy-core" }
gix = { version = "0.90.0", default-features = false }
memchr = "2.8.3"
"#;
        assert_eq!(bump(manifest, "0.3.0"), want);
    }

    #[test]
    fn versions_are_three_numbers() {
        assert!(is_version("0.3.0"));
        assert!(is_version("10.20.30"));
        assert!(!is_version("0.3"));
        assert!(!is_version("0.3.0-rc1"));
        assert!(!is_version("v0.3.0"));
    }
}
