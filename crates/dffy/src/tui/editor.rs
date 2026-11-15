//! Opening a file at a line in `$VISUAL` or `$EDITOR`.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus};

/// `$VISUAL`, then `$EDITOR`, then `vi`, as git picks.
pub(super) fn command() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "vi".to_owned())
}

/// The arguments that open `path` at `line`, by the editor's convention:
/// `+line path` for terminal editors, `path:line` for GUI ones. Editors
/// with no known convention just get the path.
pub(super) fn args(editor: &str, path: &Path, line: usize) -> Vec<OsString> {
    let program = editor.split_whitespace().next().unwrap_or_default();
    let name = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let at_line = || {
        let mut arg = path.as_os_str().to_owned();
        arg.push(format!(":{line}"));
        arg
    };
    match name {
        "vi" | "vim" | "nvim" | "gvim" | "mvim" | "hx" | "helix" | "emacs" | "emacsclient"
        | "nano" | "pico" | "micro" | "kak" | "joe" | "mg" | "ne" => {
            vec![format!("+{line}").into(), path.into()]
        }
        "code" | "code-insiders" | "codium" | "cursor" => vec!["-g".into(), at_line()],
        "zed" | "zeditor" | "subl" | "sublime_text" => vec![at_line()],
        _ => vec![path.into()],
    }
}

/// Runs the editor through `sh` (so `$EDITOR` may carry arguments, as with
/// git) with the terminal as its input, and waits for it.
pub(super) fn open(path: &Path, line: usize, tty: File) -> io::Result<ExitStatus> {
    let editor = command();
    Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(&editor)
        .args(args(&editor, path, line))
        .stdin(tty)
        .status()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(editor: &str) -> Vec<String> {
        args(editor, Path::new("src/lib.rs"), 42)
            .into_iter()
            .map(|a| a.into_string().expect("utf-8"))
            .collect()
    }

    #[test]
    fn line_conventions_by_editor() {
        assert_eq!(strings("nvim"), ["+42", "src/lib.rs"]);
        assert_eq!(strings("/usr/bin/hx"), ["+42", "src/lib.rs"]);
        assert_eq!(strings("emacsclient -t"), ["+42", "src/lib.rs"]);
        assert_eq!(strings("code --wait"), ["-g", "src/lib.rs:42"]);
        assert_eq!(strings("zed"), ["src/lib.rs:42"]);
        assert_eq!(strings("ed"), ["src/lib.rs"]);
    }
}
