//! git as a subprocess, run where the user's worktree is: the PR's code
//! only ever arrives by fetch, and an imported comment's anchor is read from
//! the commit GitHub gives for it, not from the diff on screen.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::ops::Range;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

/// Lines of context an anchor keeps on each side. Must match dffy's own
/// anchors: placement compares them whole.
const CONTEXT: usize = 2;

pub(crate) fn git(args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        // dffy runs the plugin with no terminal to ask for credentials on.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("can't run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
}

/// Where a fetched PR head is kept. A ref rather than `FETCH_HEAD`, so the
/// next fetch doesn't drop it and gc doesn't prune it before a reopen.
pub(crate) fn pull_ref(pr: u64) -> String {
    format!("refs/dffy/pull/{pr}")
}

pub(crate) fn fetch_pulls(prs: &[u64]) -> Result<(), String> {
    let specs: Vec<String> = prs
        .iter()
        .map(|pr| format!("+refs/pull/{pr}/head:{}", pull_ref(*pr)))
        .collect();
    let mut args = vec!["fetch", "--quiet", "--no-tags", "origin"];
    args.extend(specs.iter().map(String::as_str));
    git(&args).map(drop)
}

/// Fetches `commits` from origin by oid, onto no ref. A commit origin
/// lacks fails the whole fetch and git names it, so the fetch retries
/// without the named ones. Returns whether any arrived.
pub(crate) fn fetch_commits(commits: &[&str]) -> bool {
    let mut wanted = commits.to_vec();
    while !wanted.is_empty() {
        let mut args = vec![
            "fetch",
            "--quiet",
            "--no-tags",
            "--end-of-options",
            "origin",
        ];
        args.extend(&wanted);
        let Err(e) = git(&args) else {
            return true;
        };
        // Past the echoed command, which names every oid.
        let said = e.split_once(": ").map_or("", |(_, said)| said);
        let before = wanted.len();
        wanted.retain(|c| !said.contains(c));
        if wanted.len() == before {
            return false;
        }
    }
    false
}

/// The GitHub repository `origin` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Remote {
    pub owner: String,
    pub name: String,
    /// One path component naming the repository, for the session cache.
    pub key: String,
}

impl Remote {
    pub(crate) fn origin() -> Result<Self, String> {
        let url = git(&["remote", "get-url", "origin"])?;
        Self::parse(&url)
            .ok_or_else(|| format!("can't tell a GitHub repository from origin's URL {url}"))
    }

    /// `https://github.com/o/r.git`, `git@github.com:o/r` and
    /// `ssh://git@github.com/o/r` are the same repository, with one key.
    fn parse(url: &str) -> Option<Self> {
        let url = url.trim_end_matches('/');
        let url = url.strip_suffix(".git").unwrap_or(url);
        let mut parts = url.rsplit(['/', ':']);
        let name = parts.next().filter(|s| !s.is_empty())?.to_owned();
        let owner = parts.next().filter(|s| !s.is_empty())?.to_owned();
        let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
        let rest = rest
            .split_once('@')
            .filter(|(user, _)| !user.contains('/'))
            .map_or(rest, |(_, host)| host);
        let key: String = rest
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Some(Self {
            owner,
            name,
            key: key.trim_matches('_').to_owned(),
        })
    }
}

/// Which of origin's PRs a branch is the head of.
pub(crate) enum Head {
    Pull(u64),
    /// From the branch called `name` of origin, or of a fork of
    /// `fork_owner`'s.
    Branch {
        name: String,
        fork_owner: Option<String>,
    },
}

impl Head {
    /// What `gh pr checkout` set `branch` to track: the PR's own ref when
    /// you can't push to the fork, else the fork's branch, by the fork's
    /// URL or a remote of it, under a local name that may not be the
    /// branch's (`<owner>/main` for a fork's default branch). Without an
    /// upstream, the branch of origin's with its name.
    pub(crate) fn of(branch: &str, origin: &Remote) -> Self {
        let config = |key: &str| git(&["config", "--get", &format!("branch.{branch}.{key}")]).ok();
        let own = |name: &str| Self::Branch {
            name: name.to_owned(),
            fork_owner: None,
        };
        let Some(merge) = config("merge") else {
            return own(branch);
        };
        if let Some(pr) = merge
            .strip_prefix("refs/pull/")
            .and_then(|r| r.strip_suffix("/head")?.parse().ok())
        {
            return Self::Pull(pr);
        }
        // `.` is a local branch, which says nothing of a PR.
        let (Some(name), Some(remote)) = (
            merge.strip_prefix("refs/heads/"),
            config("remote").filter(|r| r != "."),
        ) else {
            return own(branch);
        };
        if remote == "origin" {
            return own(name);
        }
        let url = git(&["remote", "get-url", &remote]).unwrap_or(remote);
        match Remote::parse(&url) {
            Some(r) if r == *origin => own(name),
            Some(r) => Self::Branch {
                name: name.to_owned(),
                fork_owner: Some(r.owner),
            },
            None => own(branch),
        }
    }

    /// Whether a PR from a branch of this name is this one, by whether
    /// it's a fork's, and whose, as GitHub or a session says.
    pub(crate) fn owns(&self, cross_repository: &Value, owner: &Value) -> bool {
        match self {
            Self::Pull(_) => false,
            Self::Branch {
                fork_owner: None, ..
            } => cross_repository != true,
            Self::Branch {
                fork_owner: Some(fork),
                ..
            } => {
                cross_repository == true
                    && owner.as_str().is_some_and(|o| o.eq_ignore_ascii_case(fork))
            }
        }
    }
}

/// Files at commits, read through one `git cat-file --batch`, and merge
/// bases, both cached for a run.
#[derive(Default)]
pub(crate) struct Source {
    batch: Option<(Child, ChildStdin, BufReader<ChildStdout>)>,
    files: HashMap<String, Option<Vec<String>>>,
    merge_bases: HashMap<String, Option<String>>,
}

impl Source {
    /// The lines of `path` at `commit`, `None` when git doesn't have it.
    pub(crate) fn lines(&mut self, commit: &str, path: &str) -> Option<&[String]> {
        let spec = format!("{commit}:{path}");
        if !self.files.contains_key(&spec) {
            let lines = self
                .read(&spec)
                .filter(|(kind, _)| kind == "blob")
                .map(|(_, body)| split_lines(&body));
            self.files.insert(spec.clone(), lines);
        }
        self.files.get(&spec)?.as_deref()
    }

    /// Asks the same `cat-file`, which sees what a fetch brings after it
    /// started. Not cached, for that reason.
    pub(crate) fn has_commit(&mut self, oid: &str) -> bool {
        self.read(&format!("{oid}^{{commit}}")).is_some()
    }

    /// The type and content of the object `spec` names.
    fn read(&mut self, spec: &str) -> Option<(String, Vec<u8>)> {
        let (_, stdin, stdout) = if let Some(batch) = &mut self.batch {
            batch
        } else {
            let mut child = Command::new("git")
                .args(["cat-file", "--batch"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let stdin = child.stdin.take()?;
            let stdout = BufReader::new(child.stdout.take()?);
            self.batch.insert((child, stdin, stdout))
        };
        writeln!(stdin, "{spec}")
            .and_then(|()| stdin.flush())
            .ok()?;
        let mut header = String::new();
        stdout.read_line(&mut header).ok()?;
        // `<oid> <type> <size>`, or `<spec> missing` with no body.
        let mut fields = header.split_whitespace().skip(1);
        let kind = fields.next()?;
        let size: usize = fields.next()?.parse().ok()?;
        let mut body = vec![0; size + 1];
        stdout.read_exact(&mut body).ok()?;
        body.truncate(size);
        Some((kind.to_owned(), body))
    }

    pub(crate) fn merge_base(&mut self, base: &str, commit: &str) -> Option<String> {
        self.merge_bases
            .entry(format!("{base} {commit}"))
            .or_insert_with(|| git(&["merge-base", "--end-of-options", base, commit]).ok())
            .clone()
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if let Some((mut child, stdin, _)) = self.batch.take() {
            drop(stdin);
            child.wait().ok();
        }
    }
}

/// A hunk of a diff: the 1-based lines it shows on each side, its changes
/// and their context.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub old: Range<u64>,
    pub new: Range<u64>,
}

impl Hunk {
    pub(crate) fn side(&self, side: &str) -> &Range<u64> {
        if side == "old" { &self.old } else { &self.new }
    }
}

/// The hunks of `base...head` with `context` lines around each change, in
/// one `git diff` for every file, by each file's path at `head` (at the
/// merge base, for a deleted one). A file changed without hunks, renamed
/// only or binary, isn't in it. Two hunks one line apart are one, as
/// GitHub draws them.
pub(crate) fn hunks(
    base: &str,
    head: &str,
    context: u64,
) -> Result<HashMap<String, Vec<Hunk>>, String> {
    let diff = git(&[
        "-c",
        "core.quotePath=false",
        "diff",
        &format!("-U{context}"),
        "--inter-hunk-context=1",
        "--diff-algorithm=myers",
        "--find-renames",
        "--no-prefix",
        "--no-relative",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--end-of-options",
        &format!("{base}...{head}"),
        "--",
    ])?;
    Ok(parse_hunks(&diff))
}

fn parse_hunks(diff: &str) -> HashMap<String, Vec<Hunk>> {
    let mut files: HashMap<String, Vec<Hunk>> = HashMap::new();
    let (mut old, mut path, mut in_header) = ("", None, false);
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            (old, path, in_header) = ("", None, true);
        } else if line.starts_with("@@ -") {
            in_header = false;
            if let (Some(file), Some(hunk)) = (&path, header(line)) {
                files.entry(String::clone(file)).or_default().push(hunk);
            }
        } else if in_header {
            // Only a header's `---` and `+++` lines name files; in a hunk
            // they're removed or added lines starting with `--` or `++`.
            // git ends a name holding a space with a tab, for patch(1).
            let name = |n: &str| n.strip_suffix('\t').unwrap_or(n).to_owned();
            if let Some(from) = line.strip_prefix("--- ") {
                old = from;
            } else if let Some(to) = line.strip_prefix("+++ ") {
                path = Some(if to == "/dev/null" {
                    name(old)
                } else {
                    name(to)
                });
            }
        }
    }
    files
}

/// The hunks of one file's patch, as GitHub sends it: its `@@` lines.
pub(crate) fn patch_hunks(patch: &str) -> Vec<Hunk> {
    patch.lines().filter_map(header).collect()
}

/// `@@ -1,3 +1,4 @@`, `None` for any other line.
fn header(line: &str) -> Option<Hunk> {
    let mut sides = line.strip_prefix("@@ -")?.split(' ');
    Some(Hunk {
        old: span(sides.next()?)?,
        new: span(sides.next()?.strip_prefix('+')?)?,
    })
}

/// `start,count` of a hunk header as lines; `count` is 1 when left out.
fn span(spec: &str) -> Option<Range<u64>> {
    let (start, count) = spec.split_once(',').unwrap_or((spec, "1"));
    let start: u64 = start.parse().ok()?;
    Some(start..start + count.parse::<u64>().ok()?)
}

/// A file's lines without their `\n`, decoded as dffy decodes them.
fn split_lines(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_suffix('\n').unwrap_or(&text);
    text.split('\n').map(str::to_owned).collect()
}

/// The anchor of the 1-based lines `first..=last`, `None` when they aren't
/// all in the file.
pub(crate) fn anchor(lines: &[String], first: usize, last: usize) -> Option<Value> {
    if first == 0 || first > last || last > lines.len() {
        return None;
    }
    let (start, end) = (first - 1, last);
    Some(json!({
        "before": lines[start.saturating_sub(CONTEXT)..start],
        "lines": lines[start..end],
        "after": lines[end..(end + CONTEXT).min(lines.len())],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_url_form_of_a_repository_has_one_key() {
        let https = Remote::parse("https://github.com/acme/widgets.git");
        assert_eq!(
            https,
            Some(Remote {
                owner: "acme".to_owned(),
                name: "widgets".to_owned(),
                key: "github.com_acme_widgets".to_owned(),
            })
        );
        assert_eq!(Remote::parse("git@github.com:acme/widgets"), https);
        assert_eq!(Remote::parse("ssh://git@github.com/acme/widgets/"), https);
        assert_eq!(Remote::parse("widgets"), None);
    }

    #[test]
    fn hunks_are_keyed_by_the_new_name_and_ignore_header_lookalikes_in_hunks() {
        let diff = "diff --git a b\n\
                    --- a b\t\n\
                    +++ new name\t\n\
                    @@ -1,3 +1,4 @@ fn f() {\n\
                    \x20x\n\
                    +++ added\n\
                    --- removed\n\
                    @@ -9 +10,0 @@\n\
                    -gone\n\
                    diff --git deleted deleted\n\
                    --- deleted\n\
                    +++ /dev/null\n\
                    @@ -1,2 +0,0 @@\n\
                    diff --git same.rs renamed.rs\n\
                    similarity index 100%\n";
        let hunks = parse_hunks(diff);
        assert_eq!(
            hunks["new name"],
            [
                Hunk {
                    old: 1..4,
                    new: 1..5
                },
                Hunk {
                    old: 9..10,
                    new: 10..10
                },
            ]
        );
        assert_eq!(
            hunks["deleted"],
            [Hunk {
                old: 1..3,
                new: 0..0
            }]
        );
        assert_eq!(hunks.len(), 2);
    }

    #[test]
    fn an_anchor_keeps_up_to_two_lines_around_and_none_past_the_end() {
        let lines = split_lines(b"a\nb\r\nc\nd\n");
        assert_eq!(lines, ["a", "b\r", "c", "d"]);
        assert_eq!(
            anchor(&lines, 1, 2),
            Some(json!({"before": [], "lines": ["a", "b\r"], "after": ["c", "d"]}))
        );
        assert_eq!(anchor(&lines, 4, 5), None);
        assert_eq!(split_lines(b""), Vec::<String>::new());
        assert_eq!(split_lines(b"\n"), [""]);
    }
}
