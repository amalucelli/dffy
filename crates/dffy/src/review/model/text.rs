//! A comment's text as the frontends show it without styles: the first
//! line of a folded thread, plain lines, its age and a thread to paste;
//! and the Markdown flavor comments are written in.

use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, TagEnd};

use super::Thread;
use crate::render::plain;
use crate::review::who;

/// The Markdown comments are written in: `CommonMark` with GitHub's
/// tables, strikethrough and task lists.
pub(crate) fn markdown_options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// What a folded thread and the comments panel show of a comment: the
/// first line of its text as rendered, without styles; escaped.
pub(crate) fn summary(text: &str) -> String {
    let mut out = String::new();
    // A block's comment may run over several of its lines.
    let mut html = String::new();
    for event in Parser::new_ext(text, markdown_options()) {
        match &event {
            Event::Text(t) | Event::Code(t) => out.push_str(t),
            Event::InlineHtml(h) => out.push_str(&uncomment(h)),
            Event::Html(h) => html.push_str(h),
            Event::End(TagEnd::HtmlBlock) => out.push_str(&uncomment(&std::mem::take(&mut html))),
            Event::TaskListMarker(done) => out.push_str(checkbox(*done)),
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::Item
                | TagEnd::CodeBlock
                | TagEnd::TableCell,
            ) => out.push('\n'),
            _ => {}
        }
        if let Some((first, _)) = out.trim_start().split_once('\n') {
            return plain(first.trim());
        }
    }
    plain(out.trim())
}

/// A comment's text for a frontend that shows it without styles: its lines
/// as rendered, paragraphs and blocks a blank line apart, list items and
/// quotes marked, code indented as written, a link's address after its
/// text, HTML comments left out; each line [`plain`], unwrapped.
pub(crate) fn plain_text(text: &str) -> Vec<String> {
    use pulldown_cmark::Tag;
    let mut l = Lines::default();
    let (mut lists, mut links): (Vec<Option<u64>>, Vec<(String, usize)>) = (Vec::new(), Vec::new());
    let (mut code, mut html) = (false, String::new());
    for event in Parser::new_ext(text, markdown_options()) {
        match event {
            Event::Text(t) if code => {
                for line in t.split_inclusive('\n') {
                    l.push("    ");
                    l.push(line.strip_suffix('\n').unwrap_or(line));
                    if line.ends_with('\n') {
                        l.end();
                    }
                }
            }
            Event::Text(t) | Event::Code(t) => l.push(&t),
            Event::InlineHtml(h) => l.push(&uncomment(&h)),
            Event::Html(h) => html.push_str(&h),
            Event::End(TagEnd::HtmlBlock) => {
                for line in uncomment(&std::mem::take(&mut html)).lines() {
                    if !line.trim().is_empty() {
                        l.push(line);
                        l.end();
                    }
                }
            }
            Event::TaskListMarker(done) => l.push(checkbox(done)),
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(TagEnd::TableRow | TagEnd::TableHead | TagEnd::Item) => l.end(),
            Event::End(TagEnd::TableCell) => l.push(" | "),
            Event::Rule => {
                l.blank();
                l.push("---");
                l.blank();
            }
            Event::Start(Tag::List(start)) => {
                l.end();
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
                if lists.is_empty() {
                    l.blank();
                }
            }
            Event::Start(Tag::Item) => {
                l.end();
                let marker = match lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => "- ".to_owned(),
                };
                l.push(&"  ".repeat(lists.len().saturating_sub(1)));
                l.push(&marker);
            }
            Event::End(TagEnd::Paragraph) if !lists.is_empty() => l.end(),
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Table) => l.blank(),
            Event::Start(Tag::CodeBlock(_)) => {
                l.end();
                code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                code = false;
                l.blank();
            }
            Event::Start(Tag::BlockQuote(_)) => {
                l.end();
                l.quote += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                l.end();
                l.quote = l.quote.saturating_sub(1);
                l.blank();
            }
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                links.push((dest_url.into_string(), l.line.len()));
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                if let Some((dest, from)) = links.pop()
                    && !dest.is_empty()
                    && l.line.get(from..) != Some(dest.as_str())
                {
                    l.push(&format!(" <{dest}>"));
                }
            }
            _ => {}
        }
    }
    l.end();
    while l.out.last().is_some_and(String::is_empty) {
        l.out.pop();
    }
    l.out.iter().map(|line| plain(line)).collect()
}

/// The lines [`plain_text`] makes, the one being written last.
#[derive(Default)]
struct Lines {
    out: Vec<String>,
    line: String,
    /// `line` has its quote marks, so it's a line even while empty.
    started: bool,
    /// Block quotes the text is in.
    quote: usize,
}

impl Lines {
    fn push(&mut self, text: &str) {
        if !self.started {
            self.line.push_str(&"> ".repeat(self.quote));
            self.started = true;
        }
        self.line.push_str(text);
    }

    fn end(&mut self) {
        if self.started {
            self.out.push(std::mem::take(&mut self.line));
            self.started = false;
        }
    }

    /// Ends the line, then parts what comes next with a blank one.
    fn blank(&mut self) {
        self.end();
        if self.out.last().is_some_and(|l| !l.is_empty()) {
            self.out.push(String::new());
        }
    }
}

/// How long ago `created` was at `now` (seconds since 1970): `just now`,
/// `5m ago` up to `29d ago`, then its date.
pub(crate) fn age(created: &str, now: u64) -> String {
    let date = || created.get(..10).unwrap_or(created).to_owned();
    let Some(then) = epoch(created) else {
        return date();
    };
    match now.saturating_sub(then) {
        s if s < 60 => "just now".to_owned(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 30 * 86_400 => format!("{}d ago", s / 86_400),
        _ => date(),
    }
}

pub(crate) fn checkbox(done: bool) -> &'static str {
    if done { "☑ " } else { "☐ " }
}

/// `html` less its comments, an unclosed one to the end.
pub(crate) fn uncomment(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        rest = after.find("-->").map_or("", |end| &after[end + 3..]);
    }
    out.push_str(rest);
    out
}

/// `thread` as text to paste: each comment as written, under a line of its
/// author and the day it was written, a blank line apart.
pub(crate) fn thread_text(thread: &Thread) -> String {
    let comments: Vec<String> = thread
        .comments
        .iter()
        .map(|comment| {
            let day = comment.created.get(..10).unwrap_or(&comment.created);
            let text = comment.text.trim_end_matches(['\n', '\r']);
            format!("{} · {day}\n{text}", who(&comment.author))
        })
        .collect();
    comments.join("\n\n")
}

/// Seconds since 1970, for the threads' ages; fixed in tests, at noon on
/// 2026-11-17 UTC, so snapshots keep their ages.
#[cfg(not(test))]
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
pub(crate) fn now() -> u64 {
    1_794_916_800
}

/// Whether `url` is a web link, the only kind a thread's link opens as:
/// handed to the platform's opener, anything else could start a program.
pub(crate) fn is_web(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Seconds since 1970 at `created`, an RFC 3339 time; `None` when it isn't
/// one.
pub(crate) fn epoch(created: &str) -> Option<u64> {
    let num = |r: Range<usize>| created.get(r)?.parse::<u64>().ok();
    let b = created.as_bytes();
    if (b.get(4), b.get(7), b.get(13), b.get(16))
        != (Some(&b'-'), Some(&b'-'), Some(&b':'), Some(&b':'))
    {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let secs = num(11..13)? * 3600 + num(14..16)? * 60 + num(17..19)?;
    let rest = created.get(19..)?;
    let rest = rest
        .strip_prefix('.')
        .map_or(rest, |r| r.trim_start_matches(|c: char| c.is_ascii_digit()));
    let offset = match rest.as_bytes() {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), ..] if rest.len() == 6 => {
            let n = |r: Range<usize>| rest.get(r)?.parse::<i64>().ok();
            let minutes = n(1..3)? * 60 + n(4..6)?;
            if *sign == b'+' { minutes } else { -minutes }
        }
        _ => return None,
    };
    // Howard Hinnant's days_from_civil, for years from 1970.
    let y = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day.checked_sub(1)?;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = (era * 146_097 + doe).checked_sub(719_468)?;
    let utc = i64::try_from(days * 86_400 + secs).ok()? - offset * 60;
    u64::try_from(utc).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folded thread's line: the first line of text, past HTML comments and
    /// without its markup.
    #[test]
    fn summaries_are_the_first_line_of_text() {
        assert_eq!(
            summary(
                "<!-- cr-status -->\n<!-- cr-state\n{} -->\n\n@ana **Needs changes** at x.\nmore"
            ),
            "@ana Needs changes at x."
        );
        assert_eq!(summary("## Title\n\nbody"), "Title");
        assert_eq!(summary("- [x] Feature\n- [ ] Bug"), "☑ Feature");
        assert_eq!(summary("```rust\nfn main() {}\n```"), "fn main() {}");
        assert_eq!(summary("plain\r\nsecond"), "plain");
        assert_eq!(summary("a\u{1b}[31m"), "a^[[31m");
        assert_eq!(summary("<!-- nothing else -->"), "");
    }

    /// A comment's text as plain text for a box: paragraphs a blank line
    /// apart, list items marked, code kept as written, a link's address
    /// after its text, HTML comments left out.
    #[test]
    fn markdown_reads_as_plain_text() {
        let text = "Use **checked** math.\nSee [docs](https://x.y).\n\n- [x] one\n- two\n\n1. first\n\n```rust\nlet a = 1;\n    b\n```\n\n> quoted\n<!-- hidden -->";
        assert_eq!(
            plain_text(text),
            [
                "Use checked math.",
                "See docs <https://x.y>.",
                "",
                "- ☑ one",
                "- two",
                "",
                "1. first",
                "",
                "    let a = 1;",
                "        b",
                "",
                "> quoted",
            ]
        );
        assert_eq!(plain_text("a\u{1b}[31m"), ["a^[[31m"]);
    }
}
