//! Working-tree files as git reads them. Content goes through git's to-git
//! conversion (end of line, `ident`, `working-tree-encoding`, clean and
//! process filters) before it's hashed or shown, so a checkout that git
//! calls clean isn't listed as modified. A tracked path is read only when
//! every directory leading to it is a real directory: git never follows a
//! symbolic link in the middle of a path, and neither does a diff of it.

use std::ffi::OsStr;
use std::fs;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use gix::bstr::BStr;
use gix::filter::plumbing::pipeline::CrlfRoundTripCheck;
use gix::filter::plumbing::pipeline::convert::ToGitOutcome;
use gix::objs::Find as _;

/// A thread's [`ToGit`], built when a file first needs it.
#[derive(Default)]
pub(super) enum Conversion {
    #[default]
    Unbuilt,
    Ready(Box<ToGit>),
    /// It couldn't be built; files are taken as read.
    Unavailable,
}

impl Conversion {
    /// The conversion, built with `build` on first use.
    pub(super) fn get(
        &mut self,
        build: impl FnOnce() -> super::Result<ToGit>,
    ) -> Option<&mut ToGit> {
        if let Self::Unbuilt = self {
            *self = build().map_or(Self::Unavailable, |to_git| Self::Ready(Box::new(to_git)));
        }
        match self {
            Self::Ready(to_git) => Some(to_git),
            Self::Unbuilt | Self::Unavailable => None,
        }
    }
}

/// Git's to-git conversion, for one thread.
pub(super) struct ToGit {
    repo: gix::Repository,
    pipeline: gix::filter::plumbing::Pipeline,
    attributes: gix::worktree::Stack,
    /// Read when a conversion first needs an index blob (an `autocrlf`
    /// file that has carriage returns in the index is left alone) and the
    /// caller has no index at hand.
    index: Option<gix::index::File>,
}

impl ToGit {
    /// `index` supplies the `.gitattributes` files that aren't checked out;
    /// with an empty one, only those on disk apply.
    pub(super) fn new(repo: gix::Repository, index: &gix::index::State) -> super::Result<Self> {
        use gix::worktree::stack::state::attributes::Source;
        let attributes = repo
            .attributes_only(index, Source::WorktreeThenIdMapping)?
            .detach();
        let mut options = gix::filter::Pipeline::options(&repo)?;
        // git checks this when adding a file. Nothing is added here, and the
        // check would turn a file with mixed line endings into an error.
        options.crlf_roundtrip_check = CrlfRoundTripCheck::Skip;
        let pipeline = gix::filter::plumbing::Pipeline::new(
            repo.command_context()?,
            repo.object_hash(),
            options,
        );
        Ok(Self {
            repo,
            pipeline,
            attributes,
            index: None,
        })
    }

    /// Replaces `content`, the bytes of the worktree file `path`, with what
    /// git would store for it. When a filter can't run, `content` stays as
    /// read and the reason comes back.
    pub(super) fn convert(
        &mut self,
        path: &BStr,
        content: &mut Vec<u8>,
        index: Option<&gix::index::State>,
    ) -> Option<String> {
        let mut out = Vec::new();
        match self.run(path, content, index, &mut out) {
            Ok(false) => None,
            Ok(true) => {
                *content = out;
                None
            }
            Err(reason) => Some(reason),
        }
    }

    /// Writes the converted bytes to `out`; `false` when no conversion
    /// applies to `path`.
    fn run(
        &mut self,
        path: &BStr,
        src: &[u8],
        index: Option<&gix::index::State>,
        out: &mut Vec<u8>,
    ) -> Result<bool, String> {
        let Self {
            repo,
            pipeline,
            attributes,
            index: own,
        } = self;
        let platform = attributes
            .at_entry(path, None, &repo.objects)
            .map_err(|e| e.to_string())?;
        let outcome = pipeline
            .convert_to_git(
                src,
                Path::new(OsStr::from_bytes(path)),
                &mut |_, attrs| {
                    platform.matching_attributes(attrs);
                },
                &mut |buf| {
                    if index.is_none() && own.is_none() {
                        *own = gix::index::File::at_or_default(
                            repo.index_path(),
                            repo.object_hash(),
                            true,
                            gix::index::decode::Options::default(),
                        )
                        .ok();
                    }
                    let blob = index
                        .or(own.as_deref())
                        .and_then(|state| state.entry_by_path(path))
                        .and_then(|e| repo.objects.try_find(&e.id, buf).ok().flatten())
                        .filter(|o| o.kind == gix::objs::Kind::Blob);
                    Ok(blob.map(drop))
                },
            )
            .map_err(|e| e.to_string())?;
        match outcome {
            ToGitOutcome::Unchanged(_) => return Ok(false),
            ToGitOutcome::Buffer(converted) => out.extend_from_slice(converted),
            ToGitOutcome::Process(mut read) => {
                read.read_to_end(out).map_err(|e| e.to_string())?;
            }
        }
        Ok(true)
    }
}

/// The last directory found to be a real directory, so that paths in index
/// order cost an lstat per new directory rather than per file.
#[derive(Default)]
pub(super) struct Leading {
    /// Repository-relative, without a trailing slash; empty for none.
    known: Vec<u8>,
}

impl Leading {
    /// Whether every directory leading to the repository-relative `path` is
    /// a real directory under `root`: none is a symbolic link, a file, or
    /// missing. The root itself may be reached through links.
    pub(super) fn real(&mut self, root: &Path, path: &[u8]) -> bool {
        let Some(slash) = path.iter().rposition(|&b| b == b'/') else {
            return true;
        };
        let dir = &path[..slash];
        let common = dir
            .iter()
            .zip(&self.known)
            .take_while(|(a, b)| a == b)
            .count();
        let whole = |s: &[u8]| s.len() == common || s[common] == b'/';
        if common == dir.len() && whole(&self.known) {
            // The directory, or one above a directory, already checked.
            return true;
        }
        // Where the first component not known to be real starts.
        let mut start = if common > 0 && whole(dir) && whole(&self.known) {
            common + 1
        } else {
            dir[..common]
                .iter()
                .rposition(|&b| b == b'/')
                .map_or(0, |i| i + 1)
        };
        let mut full = root.as_os_str().as_bytes().to_vec();
        full.push(b'/');
        let base = full.len();
        loop {
            let end = dir[start..]
                .iter()
                .position(|&b| b == b'/')
                .map_or(dir.len(), |i| start + i);
            full.truncate(base);
            full.extend_from_slice(&dir[..end]);
            let meta = fs::symlink_metadata(Path::new(OsStr::from_bytes(&full)));
            if !meta.is_ok_and(|m| m.is_dir()) {
                return false;
            }
            if end == dir.len() {
                break;
            }
            start = end + 1;
        }
        self.known.clear();
        self.known.extend_from_slice(dir);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::Leading;

    #[test]
    fn a_symlinked_directory_anywhere_in_the_path_is_caught() {
        let root = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("a/b")).expect("dirs");
        std::fs::create_dir_all(outside.path().join("c")).expect("dirs");
        std::os::unix::fs::symlink(outside.path(), root.path().join("a/link")).expect("link");
        std::fs::write(root.path().join("file"), "").expect("file");

        let mut leading = Leading::default();
        assert!(leading.real(root.path(), b"top"));
        assert!(leading.real(root.path(), b"a/b/x"));
        // Cached: a sibling in a known directory, and one above it.
        assert!(leading.real(root.path(), b"a/b/y"));
        assert!(leading.real(root.path(), b"a/z"));
        assert!(!leading.real(root.path(), b"a/link/c/x"));
        assert!(!leading.real(root.path(), b"a/link/x"));
        assert!(!leading.real(root.path(), b"file/x"));
        assert!(!leading.real(root.path(), b"missing/x"));
        assert!(leading.real(root.path(), b"a/b/x"));
    }
}
