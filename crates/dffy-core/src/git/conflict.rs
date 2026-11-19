//! A conflicted file's conflicts as git left them: its sides in the index
//! (stages 1 to 3) and the regions its markers bound in the working tree;
//! and writing the file a set of choices resolves it to.

use std::io::Write as _;
use std::os::unix::ffi::OsStrExt;

use gix::bstr::{BStr, ByteSlice};

use super::{BINARY_SNIFF_BYTES, Error, FileDiff, Mode, Repo, Result, Side, Status};
use crate::conflict::{self, Choices, Conflicts, Sides};
use crate::diff::Text;

/// An index stage's blob and mode.
type Stage = Option<(gix::ObjectId, Mode)>;

impl Repo {
    /// The conflicts of `diff`'s file, a conflicted one whose new side is
    /// the working-tree file, or why they can't be resolved by region: a
    /// file deleted on one side (saying which), a binary file, a symbolic
    /// link or submodule, a file too large to diff or shown as on disk.
    /// The index's sides settle regions whose markers read two ways (see
    /// [`conflict::regions`]). Never fails; a read failure is the refusal.
    #[must_use]
    pub fn conflicts(&self, diff: &FileDiff) -> Conflicts {
        let refused = |why: String| Conflicts {
            regions: Vec::new(),
            refused: Some(why),
        };
        let [base, ours, theirs] = match self.stages(diff.change.path.as_bstr()) {
            Ok(stages) => stages,
            Err(e) => return refused(format!("can't read the conflict: {e}")),
        };
        if let Some(why) = refusal(diff, [base, ours, theirs]) {
            return refused(why.to_owned());
        }
        let load = |stage: Stage| -> Result<Option<Text>> {
            stage
                .map(|(id, mode)| {
                    self.load(BStr::new(""), &Side::Object { id, mode })
                        .map(Text::new)
                })
                .transpose()
        };
        let texts = match (load(base), load(ours), load(theirs)) {
            (Ok(base), Ok(ours), Ok(theirs)) => [base, ours, theirs],
            (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => {
                return refused(format!("can't read the conflict: {e}"));
            }
        };
        let binary = |t: &Text| t.bytes()[..t.bytes().len().min(BINARY_SNIFF_BYTES)].contains(&0);
        if texts.iter().flatten().any(binary) {
            return refused(BINARY.to_owned());
        }
        let [base, ours, theirs] = &texts;
        let sides = Sides {
            base: base.as_ref(),
            ours: ours.as_ref(),
            theirs: theirs.as_ref(),
        };
        Conflicts {
            regions: conflict::regions(&diff.new, &sides),
            refused: None,
        }
    }

    /// Writes what `choices` resolve the working-tree file `path` to, in
    /// place of it: through a file beside it renamed over it, keeping its
    /// permissions. The file is read again first, as it is on disk, and
    /// its line endings kept; it must still have the regions the choices
    /// were made on. Regions without a choice keep their markers. Returns
    /// how many did.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`] when the file's conflicts changed since the
    /// choices were made, or it's no longer a regular file; [`Error::Io`]
    /// when it can't be read or written.
    pub fn write_resolution(&self, path: &BStr, choices: &Choices) -> Result<usize> {
        let full = self.worktree_path(path)?;
        let io = |source| Error::Io {
            path: full.clone(),
            source,
        };
        // Renaming over a link would replace it with a file of its target's
        // text.
        if !std::fs::symlink_metadata(&full).map_err(io)?.is_file() {
            return Err(Error::Refused(
                "the working-tree file isn't a regular file".to_owned(),
            ));
        }
        let text = Text::new(std::fs::read(&full).map_err(io)?);
        // The sides settle the regions as they did for the file shown.
        let [base, ours, theirs] = self.stages(path)?;
        let load = |stage: Stage| {
            stage.and_then(|(id, mode)| {
                self.load(BStr::new(""), &Side::Object { id, mode })
                    .ok()
                    .map(Text::new)
            })
        };
        let texts = [load(base), load(ours), load(theirs)];
        let sides = Sides {
            base: texts[0].as_ref(),
            ours: texts[1].as_ref(),
            theirs: texts[2].as_ref(),
        };
        if conflict::regions(&text, &sides) != choices.regions {
            return Err(Error::Refused(
                "the file's conflicts changed since; choose again".to_owned(),
            ));
        }
        let resolved = conflict::resolve(&text, &choices.regions, &choices.picks);
        let permissions = std::fs::metadata(&full).map_err(io)?.permissions();
        let dir = full.parent().unwrap_or(&full);
        let mut name = b".".to_vec();
        name.extend_from_slice(
            full.file_name()
                .map_or(b"file".as_slice(), OsStrExt::as_bytes),
        );
        name.extend_from_slice(format!(".dffy-{}", std::process::id()).as_bytes());
        let temp = dir.join(std::ffi::OsStr::from_bytes(&name));
        let written = (|| {
            // One a crash left goes; `create_new` won't follow a link put
            // in its place.
            drop(std::fs::remove_file(&temp));
            let mut file = std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(resolved.text.bytes())?;
            file.set_permissions(permissions)?;
            file.sync_all()?;
            std::fs::rename(&temp, &full)
        })();
        if let Err(source) = written {
            drop(std::fs::remove_file(&temp));
            return Err(io(source));
        }
        Ok(choices.picks.iter().filter(|p| p.is_none()).count())
    }

    /// The blob and mode of stages 1 (base), 2 (ours) and 3 (theirs) of
    /// `path` in the index, `None` for each it lacks.
    fn stages(&self, path: &BStr) -> Result<[Stage; 3]> {
        let index = self.load_index()?;
        let entries = index.entries();
        let first = entries.partition_point(|e| e.path(&index) < path);
        let mut stages = [None; 3];
        for e in entries[first..]
            .iter()
            .take_while(|e| e.path(&index) == path)
        {
            if let Some(slot) = (e.stage_raw() as usize)
                .checked_sub(1)
                .and_then(|i| stages.get_mut(i))
            {
                *slot = Some((e.id, Mode::from_index(e.mode).unwrap_or(Mode::File)));
            }
        }
        Ok(stages)
    }
}

const BINARY: &str =
    "can't resolve a binary file by region; git checkout --ours or --theirs picks a side";

/// Why `diff`'s file, with index stages `[base, ours, theirs]`, can't be
/// resolved by region, from what's known without reading the sides.
fn refusal(diff: &FileDiff, [_, ours, theirs]: [Stage; 3]) -> Option<&'static str> {
    if diff.change.status != Status::Unmerged {
        return Some("this file has no conflict");
    }
    match (ours, theirs) {
        (None, Some(_)) => {
            return Some(
                "the current side (ours) deleted this file and the incoming side changed it: \
                 git add keeps it, git rm deletes it",
            );
        }
        (Some(_), None) => {
            return Some(
                "the incoming side (theirs) deleted this file and the current side changed it: \
                 git add keeps it, git rm deletes it",
            );
        }
        (None, None) => return Some("neither side has this file: git rm deletes it"),
        (Some(_), Some(_)) => {}
    }
    let regular = |s: Stage| s.is_none_or(|(_, m)| matches!(m, Mode::File | Mode::Executable));
    if !regular(ours) || !regular(theirs) {
        return Some(
            "can't resolve a symbolic link or a submodule by region; \
             git checkout --ours or --theirs picks a side",
        );
    }
    match diff.change.new {
        Some(Side::Worktree {
            mode: Mode::File | Mode::Executable,
            ..
        }) => {}
        Some(Side::Worktree { .. }) => {
            return Some("the working-tree file isn't a regular file");
        }
        None => return Some("the working-tree file is gone: git rm deletes it, git add keeps it"),
        Some(Side::Object { .. } | Side::Path { .. }) => {
            return Some("conflicts are resolved in the working tree: plain dffy shows it");
        }
    }
    if diff.binary {
        return Some(BINARY);
    }
    if diff.too_large {
        return Some("can't resolve a file too large to diff");
    }
    if diff.note.is_some() {
        return Some("can't resolve a file whose filter failed: it's shown as on disk");
    }
    None
}
