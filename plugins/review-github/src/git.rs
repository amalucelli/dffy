//! git as a subprocess, run where the user's worktree is: the PR's code
//! only ever arrives by fetch, and an imported comment's anchor is read from
//! the commit GitHub gives for it, not from the diff on screen.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

/// Lines of context an anchor keeps on each side; dffy's own anchors keep
/// as many, and placement compares them whole.
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

pub(crate) fn has_commit(oid: &str) -> bool {
    git(&["cat-file", "-e", &format!("{oid}^{{commit}}")]).is_ok()
}

/// Where a fetched PR head is kept. A ref rather than `FETCH_HEAD`, so the
/// next fetch doesn't drop it and gc doesn't prune it before a reopen.
pub(crate) fn pull_ref(pr: u64) -> String {
    format!("refs/dffy/pull/{pr}")
}

/// Fetches the heads of `prs` from origin in one fetch.
pub(crate) fn fetch_pulls(prs: &[u64]) -> Result<(), String> {
    let specs: Vec<String> = prs
        .iter()
        .map(|pr| format!("+refs/pull/{pr}/head:{}", pull_ref(*pr)))
        .collect();
    let mut args = vec!["fetch", "--quiet", "--no-tags", "origin"];
    args.extend(specs.iter().map(String::as_str));
    git(&args).map(drop)
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
            let lines = self.read(&spec);
            self.files.insert(spec.clone(), lines);
        }
        self.files.get(&spec)?.as_deref()
    }

    fn read(&mut self, spec: &str) -> Option<Vec<String>> {
        if self.batch.is_none() {
            let mut child = Command::new("git")
                .args(["cat-file", "--batch"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let stdin = child.stdin.take()?;
            let stdout = BufReader::new(child.stdout.take()?);
            self.batch = Some((child, stdin, stdout));
        }
        let (_, stdin, stdout) = self.batch.as_mut()?;
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
        (kind == "blob").then(|| split_lines(&body))
    }

    pub(crate) fn merge_base(&mut self, base: &str, commit: &str) -> Option<String> {
        self.merge_bases
            .entry(format!("{base} {commit}"))
            .or_insert_with(|| git(&["merge-base", base, commit]).ok())
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
