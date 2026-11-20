//! Reading and writing the files behind [`Comments`]: operations applied
//! under a file's lock and read back at once, and a reload when another
//! program changed a file, told by its stat or by its bytes.

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::hash::Hasher as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use dffy_core::bstr::BString;

use super::{Comments, annotation};
use crate::review::{self, Comment, Op};

/// A file's length and a hash of its bytes: unlike its stat, a file
/// replaced within one tick of the clock, its inode reused, still differs.
pub(super) type Stamp = (usize, u64);

/// A file's modification time, size and inode.
pub(super) type Stat = (i64, i64, u64, u64);

/// How [`Comments::refresh`] tells that a file changed since the last read.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Check {
    /// By its stat: cheap, and enough for dffy's own writes, which replace
    /// the file.
    Stat,
    /// By its bytes, which another program's write changes even when it
    /// keeps the size, the inode and the time, as one in the same instant
    /// may. The same bytes written again cost no parse.
    Bytes,
}

/// The stamp of a file holding `bytes`.
fn stamp(bytes: &[u8]) -> Stamp {
    // Keyed the same on every call, unlike `RandomState`.
    let mut hasher = std::hash::DefaultHasher::new();
    hasher.write(bytes);
    (bytes.len(), hasher.finish())
}

/// What a write left: the paths whose comments changed in the read back,
/// and why reading back failed, when it did. The write landed either way,
/// so writing it again would write it twice.
#[derive(Debug, Default)]
pub(crate) struct Written {
    pub changed: Vec<BString>,
    pub unread: Option<String>,
}

/// `comments` split into the threads that show on `branch`, `None` for a
/// detached HEAD, and the rest, each judged by its first comment.
fn by_branch(comments: Vec<Comment>, branch: Option<&str>) -> (Vec<Comment>, Vec<Comment>) {
    let elsewhere: HashSet<String> = comments
        .iter()
        .filter(|c| c.in_reply_to.is_none() && !c.on_branch(branch))
        .map(|c| c.id.clone())
        .collect();
    if elsewhere.is_empty() {
        return (comments, Vec::new());
    }
    comments
        .into_iter()
        .partition(|c| !elsewhere.contains(c.in_reply_to.as_ref().unwrap_or(&c.id)))
}

impl Comments {
    /// [`Comments::reload`], unless `check` finds neither file changed since
    /// the last read, as they don't after dffy's own writes, read back as
    /// they're made.
    pub(crate) fn refresh(&mut self, check: Check) -> Result<Option<Vec<BString>>, String> {
        let stats = self.stated();
        let unchanged = match check {
            Check::Stat => stats == self.stats,
            Check::Bytes => self.stamped() == self.stamps,
        };
        if !unchanged {
            return self.reload();
        }
        self.stats = stats;
        Ok(None)
    }

    fn stated(&self) -> Vec<Option<Stat>> {
        std::iter::once(&self.path)
            .chain(&self.local)
            .map(|path| {
                let m = std::fs::metadata(path).ok()?;
                Some((m.mtime(), m.mtime_nsec(), m.size(), m.ino()))
            })
            .collect()
    }

    fn stamped(&self) -> Vec<Option<Stamp>> {
        std::iter::once(&self.path)
            .chain(&self.local)
            .map(|path| review::read_bytes(path).ok().flatten().map(|b| stamp(&b)))
            .collect()
    }

    /// Reads the files back after a write of dffy's own.
    pub(super) fn reread(&mut self) -> Written {
        match self.reload() {
            Ok(changed) => Written {
                changed: changed.unwrap_or_default(),
                unread: None,
            },
            Err(e) => Written {
                changed: Vec::new(),
                unread: Some(e),
            },
        }
    }

    /// Reads the files again. Returns the paths whose comments changed,
    /// `None` when nothing did; on an error in either the last read stays.
    pub(crate) fn reload(&mut self) -> Result<Option<Vec<BString>>, String> {
        // Before the read and of the bytes parsed, so a write landing after
        // either shows as a change next time. Kept on an error too, so a
        // broken file is reported once, not on every event.
        self.stats = self.stated();
        let read = |path: &Path| match review::read_bytes(path) {
            Ok(bytes) => (
                bytes.as_deref().map(stamp),
                review::parse_at(path, bytes.as_deref()),
            ),
            Err(e) => (None, Err(e)),
        };
        let (stamp, review) = read(&self.path);
        let local = self.local.as_deref().map(read);
        self.stamps = std::iter::once(stamp)
            .chain(local.as_ref().map(|(stamp, _)| *stamp))
            .collect();
        let mut review = review?;
        let mut from_local = HashSet::new();
        let mut others = Vec::new();
        let mut others_local = HashSet::new();
        if !self.session {
            let (here, elsewhere) =
                by_branch(std::mem::take(&mut review.comments), self.branch.as_deref());
            review.comments = here;
            others = elsewhere;
        }
        if let Some((_, drafts)) = local {
            let drafts = drafts?.comments;
            // Yours only: posting moves these threads, and someone else's
            // would go out under your name.
            let roots: HashSet<String> = drafts
                .iter()
                .filter(|c| c.in_reply_to.is_none() && c.reference.is_none() && c.author.is_empty())
                .map(|c| c.id.clone())
                .collect();
            let drafts = drafts
                .into_iter()
                .filter(|c| {
                    c.reference.is_none() && roots.contains(c.in_reply_to.as_ref().unwrap_or(&c.id))
                })
                .collect();
            let (here, elsewhere) = by_branch(drafts, self.branch.as_deref());
            for c in here {
                from_local.insert(c.id.clone());
                review.comments.push(c);
            }
            others_local = elsewhere.iter().map(|c| c.id.clone()).collect();
            others.extend(elsewhere);
        }
        self.from_local = from_local;
        self.others_local = others_local;
        // Before the check: the local drafts may change when the review
        // doesn't.
        self.generation += 1;
        let others_changed = others != self.others;
        self.others = others;
        if review == self.review {
            return Ok(others_changed.then(Vec::new));
        }
        let mut by_path: HashMap<BString, Vec<usize>> = HashMap::new();
        let mut index = HashMap::with_capacity(review.comments.len());
        let mut replies: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, comment) in review.comments.iter().enumerate() {
            // The first of an id the file repeats, as a search would find.
            index.entry(comment.id.clone()).or_insert(i);
            if let Some(root) = &comment.in_reply_to {
                replies.entry(root.clone()).or_default().push(i);
            }
            by_path
                .entry(comment.path.as_bytes().into())
                .or_default()
                .push(i);
            // An old side's lines are the old file's, which a diff that
            // doesn't pair the rename lists on its own.
            if let (Some(old), Some((review::Side::Old, _))) = (&comment.old_path, comment.range())
                && *old != comment.path
            {
                by_path.entry(old.as_bytes().into()).or_default().push(i);
            }
        }
        let same = |path: &BString| {
            let old = self.by_path.get(path).map_or(&[][..], Vec::as_slice);
            let new = by_path.get(path).map_or(&[][..], Vec::as_slice);
            old.len() == new.len()
                && old
                    .iter()
                    .zip(new)
                    .all(|(&a, &b)| self.review.comments[a] == review.comments[b])
        };
        let mut changed: Vec<BString> = by_path
            .keys()
            .chain(self.by_path.keys().filter(|p| !by_path.contains_key(*p)))
            .filter(|p| !same(p))
            .cloned()
            .collect();
        changed.sort_unstable();
        self.annotations
            .replace(review.comments.iter().filter_map(annotation));
        self.review = review;
        self.by_path = by_path;
        self.index = index;
        self.replies = replies;
        self.conversation = OnceCell::new();
        self.general_len = self.general().count();
        self.remake(&changed);
        // After the threads are made, which says which local drafts are
        // out of view.
        self.key_drafts();
        Ok(Some(changed))
    }

    /// Applies `ops` to a file under its lock, then reads it back: the
    /// local review's when the first names a local draft by id (a reply
    /// in its thread, a change or a removal), or adds a thread of yours
    /// while new threads go there, else the comments file. Returns what
    /// the read back found and the id each operation touched; `Err` only
    /// when nothing was written.
    pub(crate) fn apply(&mut self, ops: Vec<Op>) -> Result<(Written, Vec<Option<String>>), String> {
        let named = match ops.first() {
            Some(
                Op::Add {
                    reply_to: Some(review::Target::Id(id)),
                    ..
                }
                | Op::Set {
                    target: review::Target::Id(id),
                    ..
                }
                | Op::Rm(review::Target::Id(id)),
            ) => Some(id),
            _ => None,
        };
        // Someone else's goes to the comments file even then, as the local
        // review's drafts shown are yours only.
        let new_thread = matches!(
            ops.first(),
            Some(Op::Add { reply_to: None, comment }) if comment.author.is_empty()
        );
        let local = named
            .is_some_and(|id| self.from_local.contains(id) || self.others_local.contains(id))
            || (new_thread && self.new_local);
        let file = match &self.local {
            Some(file) if local => file,
            _ => &self.path,
        };
        let ids = review::apply(file, ops)?;
        Ok((self.reread(), ids))
    }
}

#[cfg(test)]
mod tests {
    use super::super::Standing;
    use super::*;

    /// With the bytes deciding, the watcher's event for a write dffy already
    /// read back costs no parse, and one that keeps the size, the inode and
    /// the time, as a file replaced in the same instant may, still shows;
    /// the stat alone misses that one.
    #[test]
    fn a_refresh_reads_only_files_changed_since_the_last_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        let unchanged = "{\"version\":1,\"notice\":\"a\",\"comments\":[]}";
        std::fs::write(&path, unchanged).expect("write");
        let mut comments = Comments::new(path.clone(), None, false, None);
        assert_eq!(
            comments.refresh(Check::Stat),
            Ok(Some(Vec::new())),
            "the first read"
        );
        std::fs::write(&path, unchanged).expect("write");
        let before = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");
        assert_eq!(comments.refresh(Check::Bytes), Ok(None), "the same bytes");
        // Unreadable, with the same size, inode and time.
        std::fs::write(&path, "{\"version\":1,\"notice\":\"a\",\"comments\":[x}").expect("write");
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_modified(before).expect("set the time back");
        assert_eq!(comments.refresh(Check::Stat), Ok(None), "the same stat");
        assert!(
            comments.refresh(Check::Bytes).is_err(),
            "read once it changed"
        );
        assert_eq!(
            comments.refresh(Check::Bytes),
            Ok(None),
            "a broken file is said once"
        );
    }

    /// A write that landed is reported as written even when reading the
    /// files back failed, which is said apart: writing it again would add
    /// it twice.
    #[test]
    fn a_write_that_landed_is_written_though_the_read_back_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, local) = (dir.path().join("7.json"), dir.path().join("local.json"));
        std::fs::write(&local, "{\"version\":1,\"comments\":[]}").expect("write");
        let mut comments = Comments::new(path.clone(), Some(local.clone()), true, None);
        comments.reload().expect("reads");
        std::fs::write(&local, "{broken").expect("write");
        let add = Op::Add {
            comment: Box::new(Comment {
                path: "a.rs".to_owned(),
                text: "Once".to_owned(),
                ..Comment::default()
            }),
            reply_to: None,
        };
        let (written, ids) = comments.apply(vec![add]).expect("written");
        assert!(ids[0].is_some(), "{ids:?}");
        let unread = written.unread.unwrap_or_default();
        assert!(unread.contains("local.json"), "{unread}");
        let summary = comments.keep_summary("Two nits.").expect("kept");
        assert!(summary.unread.is_some());
        let file = review::read(&path).expect("reads");
        assert_eq!(file.comments.len(), 1, "written once");
        assert_eq!(file.summary.as_deref(), Some("Two nits."));
    }

    /// While new threads go to the local review's file, someone else's, as
    /// one added through the control API, goes to the comments file: the
    /// local one's drafts shown are yours only. It shows there as theirs,
    /// not posted.
    #[test]
    fn someone_elses_new_thread_goes_where_it_shows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, local) = (dir.path().join("7.json"), dir.path().join("local.json"));
        let mut comments = Comments::new(path, Some(local.clone()), true, None);
        comments.set_new_local(true);
        let theirs = Op::Add {
            comment: Box::new(Comment {
                path: "a.rs".to_owned(),
                text: "An agent's".to_owned(),
                author: "dffy ctl".to_owned(),
                ..Comment::default()
            }),
            reply_to: None,
        };
        comments.apply(vec![theirs]).expect("written");
        assert_eq!(review::read(&local).expect("reads").comments.len(), 0);
        assert_eq!(
            (comments.review.comments.iter())
                .find(|c| c.text == "An agent's")
                .map(|c| comments.standing(c)),
            Some(Standing::Unposted)
        );
    }
}
