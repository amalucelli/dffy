//! `--json`: the diff as JSON Lines. A header, an object per file in the
//! order printed output has them, then an end object, each encoded by
//! [`crate::encode`]. Files go through the same core steps as printed
//! output and stream through print's writer, so output starts with the
//! first file and memory stays bounded however many files there are.

use dffy_core::diff::Options;

use crate::encode::{Compared, Detail, Files, header};
use crate::output::Output;
use crate::print::Plan;
use crate::{Ctx, Error};

/// Writes the header, every file and the end object to `out`. Returns
/// whether any file has a difference that printed output would show: a
/// file only `-w` or `-b` found unchanged has an object, but isn't one.
pub(crate) fn write(
    plan: &Plan,
    compared: &Compared,
    detail: Detail,
    ctx: &Ctx,
    out: &mut Output,
) -> Result<bool, Error> {
    let options = Options {
        context: ctx.config.context.hunk_lines(false),
        whitespace: ctx.config.whitespace,
    };
    let mut files = Files::new(options, ctx.config.syntax_diff, detail, ctx.cache.as_ref());
    files.comments = plan.comments.as_ref().map(|c| (c, plan.reverse));
    let mut s = header(
        plan,
        compared,
        ctx.config.context,
        ctx.config.renames,
        &files,
    );
    s.push('\n');
    out.write_raw(s.as_bytes())?;
    out.flush()?;
    plan.stream(ctx, out, &files)?;
    let mut end = files.end();
    end.push('\n');
    out.write_raw(end.as_bytes())?;
    Ok(files.differs())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    use super::*;
    use crate::plugin::json::{Value, parse};

    #[derive(Clone, Default)]
    struct Shared(Rc<RefCell<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Every line written parses as one JSON value, with dffy's own
    /// parser: a header of version 1 first, a file object for each change,
    /// highlights and all, and the end object last with their totals.
    #[test]
    fn every_line_is_one_json_object_framed_by_header_and_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}");
        };
        let put = |path: &str, bytes: &[u8]| std::fs::write(d.join(path), bytes).expect("write");
        git(&["init", "-q", "-b", "main"]);
        put("lib.rs", b"fn main() {\n    let x = \"a\\tb\";\n}\n");
        put("bytes.bin", b"\x00\x01");
        put("latin1.txt", b"price \xa31\n");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);
        put("lib.rs", b"fn main() {\n    let x = \"a\\tc\";\r\n}");
        put("bytes.bin", b"\x00\x02");
        put("latin1.txt", b"price \xa32\n");
        put("new.txt", b"\x1b[31m\n");
        let revisions = dffy_core::git::Revisions::default();
        let target = crate::print::Target::Diff(&revisions);
        let plan = crate::print::prepare(Some(d), &target, &[], crate::print::Summary::Diff, false)
            .expect("plan");
        let buffer = Shared::default();
        let mut out = Output::to(buffer.clone());
        let compared = Compared {
            revisions: &[],
            operands: None,
        };
        let detail = Detail::Highlights;
        assert!(write(&plan, &compared, detail, &ctx(), &mut out).expect("written"));
        out.finish().expect("flushed");
        let bytes = buffer.0.borrow();
        let objects: Vec<Value> = bytes
            .split_inclusive(|&b| b == b'\n')
            .map(|line| {
                assert_eq!(line.last(), Some(&b'\n'), "every object ends its line");
                parse(line).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(line)))
            })
            .collect();
        let field = |v: &Value, key: &str| v.get(key).cloned().unwrap_or(Value::Null);
        let types: Vec<_> = objects
            .iter()
            .map(|o| field(o, "type").as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(types, ["header", "file", "file", "file", "file", "end"]);
        assert_eq!(field(&objects[0], "version").as_usize(), Some(1));
        let end = &objects[5];
        assert_eq!(field(end, "files").as_usize(), Some(4));
        for key in ["added", "removed"] {
            let sum = objects[1..5]
                .iter()
                .filter_map(|f| field(f, key).as_usize());
            assert_eq!(field(end, key).as_usize(), Some(sum.sum()), "{key}");
        }
        let lib = objects
            .iter()
            .find(|o| field(o, "path").as_str() == Some("lib.rs"))
            .expect("lib.rs");
        let hunks = field(lib, "hunks");
        let lines = field(&hunks.as_array().expect("hunks")[0], "lines");
        assert!(
            lines
                .as_array()
                .expect("lines")
                .iter()
                .any(|l| l.get("highlights").is_some()),
            "highlights are written with the flag on"
        );
    }

    /// Defaults throughout, no cache, and no terminal.
    fn ctx() -> Ctx {
        Ctx {
            config: crate::config::Config::default(),
            sources: crate::config::Sources::default(),
            last_used: Vec::new(),
            tip_shown: false,
            context_lines: 3,
            config_path: None,
            config_warnings: Vec::new(),
            theme: crate::theme::Theme::default_theme(),
            term: crate::output::Term {
                stdout_tty: false,
                stderr_tty: false,
                width: None,
            },
            color_out: false,
            color_err: false,
            cache: None,
            review_name: std::sync::OnceLock::new(),
            review: std::sync::OnceLock::new(),
            comments: None,
        }
    }
}
