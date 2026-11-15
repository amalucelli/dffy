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
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(text.len().div_ceil(3) * 4 + 8);
    out.extend_from_slice(b"\x1b]52;c;");
    for chunk in text.chunks(3) {
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
    out.push(0x07);
    out
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
}
