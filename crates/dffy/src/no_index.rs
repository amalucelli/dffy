//! `dffy A B` for files and folders outside git: when two arguments are
//! paths to compare rather than revisions (the way `git diff --no-index`
//! decides), and what the comparison is called, `git difftool`'s temporary
//! copies included.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use dffy_core::bstr::BString;
use dffy_core::git::{Compare, Endpoint, Repo};

use crate::Error;
use crate::cli::DiffArgs;

/// Labels in the status line are cut to this many characters, from the
/// left, so the end of a path (what tells two apart) stays.
const LABEL_WIDTH: usize = 32;

/// The two paths `args` asks to compare, as typed, or `None` when it asks
/// for a diff from the repository. `base` is where relative paths start.
///
/// | arguments | result |
/// |---|---|
/// | `--no-index A B` | paths; both must exist, or be `-` |
/// | `A B`, both exist, outside a repository | paths |
/// | `A B`, both exist, either outside the working tree | paths (git's rule) |
/// | `A B`, both exist inside it, neither a revision | paths |
/// | `A B`, both exist inside it, both revisions | revisions (git's preference) |
/// | `A B`, both exist inside it, one a revision | error naming the ambiguity |
/// | `-- A B` inside the working tree | pathspecs, as in git |
///
/// `-` (stdin) counts as an existing path outside the working tree.
pub(crate) fn operands(base: &Path, args: &DiffArgs) -> Result<Option<[PathBuf; 2]>, Error> {
    let after_dashes = !args.common.paths.is_empty();
    let given: Vec<PathBuf> = args
        .revs
        .iter()
        .map(PathBuf::from)
        .chain(args.common.paths.iter().cloned())
        .collect();
    let stdin = |p: &Path| p == Path::new("-");
    let exists = |p: &Path| stdin(p) || base.join(p).exists();
    if args.no_index {
        let count = given.len();
        let Ok([old, new]) = <[PathBuf; 2]>::try_from(given) else {
            return Err(Error::Fatal {
                message: format!("--no-index compares two files or folders; {count} given"),
                hint: Some("as in: dffy --no-index a.txt b.txt".to_owned()),
            });
        };
        for path in [&old, &new] {
            if !exists(path) {
                return Err(Error::fatal(format!(
                    "'{}': no such file or directory",
                    path.display()
                )));
            }
        }
        return Ok(Some([old, new]));
    }
    let selects = args.staged || args.unstaged || args.all || args.merge_base;
    let mixed = !args.revs.is_empty() && after_dashes;
    let Ok([old, new]) = <[PathBuf; 2]>::try_from(given) else {
        return Ok(None);
    };
    if selects || mixed {
        return Ok(None);
    }
    let (old_exists, new_exists) = (exists(&old), exists(&new));
    if !old_exists && !new_exists {
        return Ok(None);
    }
    let repo = Repo::discover(base).ok();
    match (old_exists, new_exists, &repo) {
        (true, true, _) => {}
        // Outside a repository they can't be revisions, so one was meant
        // as a path that isn't there.
        (true, false, None) | (false, true, None) => {
            let missing = if old_exists { &new } else { &old };
            return Err(Error::fatal(format!(
                "'{}': no such file or directory",
                missing.display()
            )));
        }
        _ => return Ok(None),
    }
    let Some(repo) = repo else {
        return Ok(Some([old, new]));
    };
    let Some(workdir) = repo.workdir().and_then(|w| w.canonicalize().ok()) else {
        return Ok(Some([old, new]));
    };
    let inside = |p: &Path| {
        !stdin(p)
            && base
                .join(p)
                .canonicalize()
                .is_ok_and(|full| full.starts_with(&workdir))
    };
    if !inside(&old) || !inside(&new) {
        return Ok(Some([old, new]));
    }
    if after_dashes {
        return Ok(None);
    }
    let revision = |p: &Path| p.to_str().is_some_and(|rev| repo.is_revision(rev));
    match (revision(&old), revision(&new)) {
        (false, false) => Ok(Some([old, new])),
        (true, true) => Ok(None),
        (old_is_revision, _) => {
            let both = if old_is_revision { &old } else { &new };
            Err(Error::Fatal {
                message: format!(
                    "'{}' is both a file and a revision; use --no-index to compare files",
                    both.display()
                ),
                hint: Some(format!(
                    "dffy --no-index {} {}",
                    old.display(),
                    new.display()
                )),
            })
        }
    }
}

/// What `git difftool` tells the tool it runs, through the environment.
#[derive(Debug, Default, Clone)]
pub(crate) struct Difftool {
    /// One file at a time: which of how many.
    pub file: Option<(String, String)>,
    /// `--dir-diff`: two temporary folders.
    pub dir_diff: bool,
    /// The file's path in the working tree, when the tool command exports
    /// `MERGED` or `BASE` (git exports `BASE` for `--extcmd`).
    pub merged: Option<PathBuf>,
}

impl Difftool {
    pub(crate) fn from_env() -> Self {
        let var = |name| std::env::var_os(name).filter(|v| !v.is_empty());
        let file = match (var("GIT_DIFF_PATH_COUNTER"), var("GIT_DIFF_PATH_TOTAL")) {
            (Some(i), Some(n)) => Some((
                i.to_string_lossy().into_owned(),
                n.to_string_lossy().into_owned(),
            )),
            _ => None,
        };
        Self {
            merged: file
                .as_ref()
                .and_then(|_| var("MERGED").or_else(|| var("BASE")))
                .map(PathBuf::from),
            dir_diff: var("GIT_DIFFTOOL_DIRDIFF").is_some(),
            file,
        }
    }
}

/// Opens the comparison of `operands` (relative to `base`), reading stdin
/// for `-`. Returns it with what the status line calls it, and the names of
/// its old and new versions, each operand as [`label`] shows it.
pub(crate) fn open(
    base: &Path,
    operands: &[PathBuf; 2],
    args: &DiffArgs,
    difftool: &Difftool,
) -> Result<(Repo, String, [String; 2]), Error> {
    let [old, new] = operands;
    if old == Path::new("-") && new == Path::new("-") {
        return Err(Error::fatal("only one side can be read from stdin"));
    }
    let endpoint = |p: &Path| -> Result<Endpoint, Error> {
        Ok(if p == Path::new("-") {
            let mut bytes = Vec::new();
            std::io::stdin()
                .lock()
                .read_to_end(&mut bytes)
                .map_err(|e| Error::fatal(format!("reading stdin: {e}")))?;
            Endpoint::Bytes(bytes)
        } else if p == Path::new("/dev/null") {
            Endpoint::Missing
        } else {
            Endpoint::Path(base.join(p))
        })
    };
    let name = display_name(old, new, difftool);
    let repo = Repo::compare(Compare {
        old: endpoint(old)?,
        new: endpoint(new)?,
        name: BString::from(name.as_os_str().as_bytes()),
        exclude: args.exclude.clone(),
        gitignore: args.gitignore,
    })?;
    let label = |p: &Path| label(base, p, difftool.dir_diff);
    let (a, b) = if args.common.view.reverse {
        (new, old)
    } else {
        (old, new)
    };
    let versions = [label(a), label(b)];
    let description = if let Some((i, n)) = &difftool.file {
        format!("{} · {i}/{n}", name.display())
    } else {
        format!("{} → {}", versions[0], versions[1])
    };
    Ok((repo, description, versions))
}

/// The path a comparison of two files is shown under, which its language is
/// detected from too: under `git difftool`, the file's working-tree path
/// when it's known, else its name without git's temporary decoration;
/// otherwise the second file's name (the first's when the second is stdin
/// or `/dev/null`).
pub(crate) fn display_name(old: &Path, new: &Path, difftool: &Difftool) -> PathBuf {
    let real = |p: &&Path| *p != Path::new("-") && *p != Path::new("/dev/null");
    if difftool.file.is_some() {
        if let Some(merged) = &difftool.merged {
            return merged.clone();
        }
        if let Some(path) = [new, old]
            .into_iter()
            .filter(real)
            .find(|p| !is_temporary(p))
        {
            return path.to_owned();
        }
    }
    let Some(path) = [new, old].into_iter().find(real) else {
        return PathBuf::from("stdin");
    };
    let name = match old_temporary_name(path) {
        Some(name) if difftool.file.is_some() => name,
        _ => path.file_name().unwrap_or(path.as_os_str()),
    };
    PathBuf::from(name)
}

/// git's temporary copy of a blob: `git-blob-XXXXXX/<name>` since git 2.21,
/// `XXXXXX_<name>` directly in the temporary directory before.
fn is_temporary(path: &Path) -> bool {
    let in_blob_dir = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|dir| dir.as_bytes().starts_with(b"git-blob-"));
    in_blob_dir || old_temporary_name(path).is_some()
}

/// The real name inside an older git's temporary name, `XXXXXX_<name>`,
/// for a file directly in the temporary directory. Anywhere else a name
/// like `server_main.go` is just a name.
fn old_temporary_name(path: &Path) -> Option<&OsStr> {
    let parent = path.parent()?;
    if parent != std::env::temp_dir() && parent != Path::new("/tmp") {
        return None;
    }
    let bytes = path.file_name()?.as_bytes();
    let prefix = bytes.get(..7).filter(|_| bytes.len() > 7)?;
    (prefix[6] == b'_' && prefix[..6].iter().all(u8::is_ascii_alphanumeric))
        .then(|| OsStr::from_bytes(&bytes[7..]))
}

/// How a compared path reads in the status line: as typed, a folder with a
/// trailing `/`, cut from the left to [`LABEL_WIDTH`]. Under `git difftool
/// --dir-diff`, just the folder's name (`left/`, `right/`).
fn label(base: &Path, path: &Path, dir_diff: bool) -> String {
    if path == Path::new("-") {
        return "stdin".to_owned();
    }
    let is_dir = base.join(path).is_dir();
    let mut text = if dir_diff && is_dir {
        path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    } else {
        path.display().to_string()
    };
    if is_dir && !text.ends_with('/') {
        text.push('/');
    }
    let count = text.chars().count();
    if count <= LABEL_WIDTH {
        return text;
    }
    let tail: String = text.chars().skip(count - (LABEL_WIDTH - 1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn difftool() -> Difftool {
        Difftool {
            file: Some(("1".to_owned(), "2".to_owned())),
            ..Difftool::default()
        }
    }

    #[test]
    fn difftool_names_come_from_the_working_tree_side_or_the_undecorated_name() {
        let name = |old: &str, new: &str, tool: &Difftool| {
            display_name(Path::new(old), Path::new(new), tool)
        };
        let tool = difftool();
        assert_eq!(
            name("/tmp/git-blob-f8Qg7r/main.rs", "src/main.rs", &tool),
            Path::new("src/main.rs")
        );
        assert_eq!(
            name(
                "/tmp/git-blob-a/Makefile",
                "/tmp/git-blob-b/Makefile",
                &tool
            ),
            Path::new("Makefile")
        );
        assert_eq!(
            name("/tmp/Ab12cd_file.rs", "/tmp/Zz9xY0_file.rs", &tool),
            Path::new("file.rs")
        );
        // Names shaped like the old decoration, outside the temporary
        // directory itself, are names.
        assert_eq!(
            name(
                "/tmp/git-blob-x/server_main.go",
                "src/server_main.go",
                &tool
            ),
            Path::new("src/server_main.go")
        );
        assert_eq!(
            name(
                "/tmp/git-blob-a/client_test.go",
                "/tmp/git-blob-b/client_test.go",
                &tool
            ),
            Path::new("client_test.go")
        );
        let merged = Difftool {
            merged: Some(PathBuf::from("lib/x.py")),
            ..difftool()
        };
        assert_eq!(name("/tmp/a/x", "/tmp/b/x", &merged), Path::new("lib/x.py"));
        let plain = Difftool::default();
        assert_eq!(name("a/one.txt", "b/two.txt", &plain), Path::new("two.txt"));
        assert_eq!(name("a/one.txt", "-", &plain), Path::new("one.txt"));
        assert_eq!(
            name("/tmp/Ab12cd_file.rs", "x", &plain),
            Path::new("x"),
            "names are left alone outside difftool"
        );
    }

    #[test]
    fn labels_keep_the_end_of_long_paths() {
        let base = Path::new("/");
        assert_eq!(label(base, Path::new("-"), false), "stdin");
        assert_eq!(label(base, Path::new("tmp"), false), "tmp/");
        let long = "a/very/long/path/to/somewhere/deep/inside/file.txt";
        let cut = label(base, Path::new(long), false);
        assert_eq!(cut.chars().count(), LABEL_WIDTH);
        assert!(
            cut.starts_with('…') && cut.ends_with("inside/file.txt"),
            "{cut}"
        );
    }
}
