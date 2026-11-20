//! dffy's own terminal layer, so the frame path and multiplexer behavior
//! are dffy's: a diffed cell grid, raw termios, and an input parser that
//! needs nothing beyond what every terminal and multiplexer passes through.

mod grid;
mod input;
mod tty;

pub(crate) use grid::{Grid, Screen};
pub(crate) use input::{Input, Key, Mods, QUERY_BACKGROUND};
pub(crate) use tty::{
    MOUSE_OFF, MOUSE_ON, Reader, Signal, Tty, install_panic_hook, on_signals, restore,
};

/// The most text one copy takes. Longer copies are refused rather than
/// cut: terminals cap OSC 52 sequences differently (base64 makes this one
/// 100,000 bytes), and a cut copy would pass for a whole one.
pub(crate) const MAX_CLIPBOARD_BYTES: usize = 75_000;

/// The OSC 52 sequence that puts `text` on the system clipboard through
/// the terminal, which works over SSH too. Nothing comes back to say it
/// worked; tmux passes it on only with `set-clipboard on`.
pub(crate) fn clipboard(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len().div_ceil(3) * 4 + 8);
    out.extend_from_slice(b"\x1b]52;c;");
    base64(text, &mut out);
    out.push(0x07);
    out
}

/// The window title (OSC 2), which names the tab: `dffy · <part> · …`.
/// The parts can be a plugin's words or a directory's name, so control
/// characters, which would end the sequence early, become spaces.
pub(crate) fn window_title(parts: &[&str]) -> Vec<u8> {
    let mut out = b"\x1b]2;dffy".to_vec();
    for part in parts {
        out.extend_from_slice(" · ".as_bytes());
        for c in part.chars() {
            let mut utf8 = [0; 4];
            let c = if c.is_control() { ' ' } else { c };
            out.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
        }
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// What dffy is doing, as it tells the terminal: the Program Status
/// Protocol's report (OSC 7501) on the terminal's root record. Terminals
/// that don't know it ignore it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Status {
    pub state: State,
    /// One line: what's going on, waited for, or finished.
    pub message: Option<String>,
    /// Percent done, of work going on.
    pub progress: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum State {
    #[default]
    Idle,
    Working,
    /// A result the user hasn't seen yet.
    Done,
    Error,
}

impl Status {
    pub(crate) fn report(&self) -> Vec<u8> {
        let state = match self.state {
            State::Idle => "idle",
            State::Working => "working",
            State::Done => "done",
            State::Error => "error",
        };
        let mut out = format!("\x1b]7501;state={state}:app=dffy").into_bytes();
        if let Some(progress) = self.progress.filter(|_| self.state == State::Working) {
            out.extend_from_slice(format!(":progress={}", progress.min(100)).as_bytes());
        }
        if let Some(message) = &self.message {
            // The protocol's cap on a message, decoded.
            const MAX_MESSAGE: usize = 2048;
            // A control character, a line break included, voids the whole
            // report.
            let mut line = String::with_capacity(message.len().min(MAX_MESSAGE));
            for c in message
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
            {
                if line.len() + c.len_utf8() > MAX_MESSAGE {
                    break;
                }
                line.push(c);
            }
            out.extend_from_slice(b":msg=");
            base64(line.as_bytes(), &mut out);
        }
        out.extend_from_slice(b"\x1b\\");
        out
    }
}

/// Standard base64, padded, appended to `out`.
fn base64(bytes: &[u8], out: &mut Vec<u8>) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ALPHABET[(n >> (18 - 6 * i) & 63) as usize]
            } else {
                b'='
            });
        }
    }
}

/// tmux's `set-clipboard` option when running inside tmux. Unless it's
/// `on`, tmux drops the OSC 52 sequences programs send.
pub(crate) fn tmux_clipboard() -> Option<String> {
    std::env::var_os("TMUX").filter(|v| !v.is_empty())?;
    let out = std::process::Command::new("tmux")
        .args(["show-options", "-sv", "set-clipboard"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn clipboard_is_an_osc_52_sequence_of_base64() {
        assert_eq!(super::clipboard(b""), b"\x1b]52;c;\x07");
        assert_eq!(
            super::clipboard(b"fn main() {\n\tx();\n"),
            b"\x1b]52;c;Zm4gbWFpbigpIHsKCXgoKTsK\x07"
        );
        assert_eq!(super::clipboard(b"ab"), b"\x1b]52;c;YWI=\x07");
        assert_eq!(super::clipboard(b"a"), b"\x1b]52;c;YQ==\x07");
    }

    #[test]
    fn the_window_title_ends_only_where_it_should() {
        assert_eq!(
            super::window_title(&["repo", "PR #7\x1b]0;evil\x07"]),
            "\x1b]2;dffy · repo · PR #7 ]0;evil \x1b\\".as_bytes()
        );
    }

    #[test]
    fn status_is_an_osc_7501_report_on_one_line() {
        use super::{State, Status};
        let status = |state, message: &str| Status {
            state,
            message: Some(message.to_owned()),
            ..Status::default()
        };
        assert_eq!(
            Status::default().report(),
            b"\x1b]7501;state=idle:app=dffy\x1b\\"
        );
        // A plugin's error that ran over lines is still one line, or the
        // terminal would drop the report.
        assert_eq!(
            status(State::Error, "listing\nchanges").report(),
            b"\x1b]7501;state=error:app=dffy:msg=bGlzdGluZyBjaGFuZ2Vz\x1b\\"
        );
        assert_eq!(
            Status {
                progress: Some(32),
                ..status(State::Working, "a")
            }
            .report(),
            b"\x1b]7501;state=working:app=dffy:progress=32:msg=YQ==\x1b\\"
        );
        // Only work going on has a percentage.
        assert_eq!(
            Status {
                progress: Some(32),
                ..status(State::Done, "a")
            }
            .report(),
            b"\x1b]7501;state=done:app=dffy:msg=YQ==\x1b\\"
        );
        // Cut to the protocol's 2048 bytes, between characters.
        let long = status(State::Working, &"·".repeat(2000)).report();
        assert_eq!(
            long.len(),
            "\x1b]7501;state=working:app=dffy:msg=".len() + 2732 + 2
        );
    }
}
