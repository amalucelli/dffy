//! Keys and mouse reports from raw terminal bytes. Only the encodings every
//! terminal and multiplexer passes through: UTF-8, C0 controls, CSI and SS3
//! keys with xterm modifiers, and SGR mouse reports. Nothing here waits for
//! a terminal to answer a query, and no extended keyboard protocol is used.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

/// Modifier bits, as xterm encodes them (minus one) in CSI parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Mods(u8);

impl Mods {
    pub(crate) const NONE: Self = Self(0);
    pub(crate) const ALT: Self = Self(2);
    pub(crate) const CTRL: Self = Self(4);

    pub(crate) fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub(crate) fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Input {
    Key(Key, Mods),
    /// Text pasted while bracketed paste is on, whole, its line ends as
    /// the terminal sent them.
    Paste(String),
    /// A wheel step: negative scrolls up. Cells are 0-based.
    Scroll {
        delta: i8,
        x: u16,
        y: u16,
    },
    /// A left-button press.
    Click {
        x: u16,
        y: u16,
    },
    /// The pointer moved with the left button held.
    Drag {
        x: u16,
        y: u16,
    },
    /// The left button was let go.
    Release {
        x: u16,
        y: u16,
    },
    /// The terminal answered the background color query.
    Background {
        rgb: (u8, u8, u8),
    },
}

/// Asks the terminal for its background color (OSC 11); the reply arrives
/// as [`Input::Background`], or never, in multiplexers that drop it.
pub(crate) const QUERY_BACKGROUND: &[u8] = b"\x1b]11;?\x1b\\";

/// How long a lone ESC waits for the rest of a sequence before it counts
/// as the Escape key. Multiplexers forward sequences whole, so this only
/// has to cover a slow link.
pub(super) const ESC_TIMEOUT_MS: u64 = 25;

/// Sequences longer than this are garbage, not a slow sender; they're
/// dropped so a stray ESC can't swallow later keys.
const MAX_SEQUENCE: usize = 64;

/// A byte-at-a-time parser: [`Parser::feed`] takes whatever a read returned,
/// keeping an incomplete sequence for the next read.
#[derive(Debug, Default)]
pub(super) struct Parser {
    buf: Vec<u8>,
    /// A timeout cut a string (an OSC reply, say) short; the rest of it is
    /// dropped through its BEL or ST rather than read as keys.
    skipping: bool,
    /// A timeout came with only a string's introducer (`ESC ]`) buffered.
    /// It stays, waiting for the rest with no timeout: read as Alt with
    /// the key, the reply's rest would arrive as keys.
    introduced: bool,
    /// A bracketed paste started: its text so far. Read to its end with
    /// no timeout and no length limit, since it's text, not a sequence.
    pasting: Option<Vec<u8>>,
}

/// The end of a bracketed paste.
const PASTE_END: &[u8] = b"\x1b[201~";

enum Parsed {
    /// Bytes consumed and what they meant, if anything.
    Done(usize, Option<Input>),
    /// A bracketed paste starts after these bytes.
    Paste(usize),
    Incomplete,
}

impl Parser {
    pub(super) fn feed(&mut self, mut bytes: &[u8], emit: &mut impl FnMut(Input)) {
        self.introduced = false;
        if self.skipping {
            bytes = &bytes[self.skip_string(bytes)..];
        }
        self.buf.extend_from_slice(bytes);
        let mut i = 0;
        while i < self.buf.len() {
            if let Some(text) = &mut self.pasting {
                let rest = &self.buf[i..];
                if let Some(at) = rest.windows(PASTE_END.len()).position(|w| w == PASTE_END) {
                    text.extend_from_slice(&rest[..at]);
                    i += at + PASTE_END.len();
                    let text = self.pasting.take().unwrap_or_default();
                    emit(Input::Paste(String::from_utf8_lossy(&text).into_owned()));
                    continue;
                }
                // What could be the start of the end stays for the next read.
                let keep = (1..PASTE_END.len())
                    .rev()
                    .find(|&n| rest.ends_with(&PASTE_END[..n]))
                    .unwrap_or(0);
                text.extend_from_slice(&rest[..rest.len() - keep]);
                i = self.buf.len() - keep;
                break;
            }
            match parse(&self.buf[i..]) {
                Parsed::Done(n, input) => {
                    if let Some(input) = input {
                        emit(input);
                    }
                    i += n;
                }
                Parsed::Paste(n) => {
                    self.pasting = Some(Vec::new());
                    i += n;
                }
                Parsed::Incomplete if self.buf.len() - i > MAX_SEQUENCE => i += 1,
                Parsed::Incomplete => break,
            }
        }
        self.buf.drain(..i);
    }

    /// Whether the start of a sequence is waiting for more bytes; call
    /// [`Parser::timeout`] when none come within [`ESC_TIMEOUT_MS`].
    pub(super) fn pending(&self) -> bool {
        !self.buf.is_empty() && !self.introduced && self.pasting.is_none()
    }

    /// No more bytes came. A lone ESC is the Escape key, ESC with one byte
    /// after it is Alt with that key unless it starts a string, which waits
    /// for its rest, and anything longer is dropped; a string's rest is
    /// dropped too when it comes.
    pub(super) fn timeout(&mut self, emit: &mut impl FnMut(Input)) {
        if self.pasting.is_some() {
            return;
        }
        match self.buf[..] {
            [0x1b, b']' | b'P' | b'_' | b'^'] => {
                self.introduced = true;
                return;
            }
            [0x1b] => emit(Input::Key(Key::Esc, Mods::NONE)),
            [0x1b, b] if b.is_ascii_graphic() => {
                emit(Input::Key(Key::Char(char::from(b)), Mods::ALT));
            }
            [0x1b, b']' | b'P' | b'_' | b'^', _, ..] => self.skipping = true,
            [first, ..] if first >= 0x80 => emit(Input::Key(INVALID, Mods::NONE)),
            _ => {}
        }
        self.buf.clear();
    }

    /// The bytes of `bytes` that finish a string cut short, through its
    /// terminator. An ESC that isn't ST ends it too and is kept.
    fn skip_string(&mut self, bytes: &[u8]) -> usize {
        for (i, &b) in bytes.iter().enumerate() {
            match b {
                0x07 => {
                    self.skipping = false;
                    return i + 1;
                }
                0x1b if bytes.get(i + 1) == Some(&b'\\') => {
                    self.skipping = false;
                    return i + 2;
                }
                0x1b => {
                    self.skipping = false;
                    return i;
                }
                _ => {}
            }
        }
        bytes.len()
    }
}

/// `n` bytes that make a key.
fn key(n: usize, key: Key, mods: Mods) -> Parsed {
    Parsed::Done(n, Some(Input::Key(key, mods)))
}

const INVALID: Key = Key::Char(char::REPLACEMENT_CHARACTER);

fn parse(b: &[u8]) -> Parsed {
    match b[0] {
        0x1b => escape(b),
        0x00..=0x1f | 0x7f => match control(b[0]) {
            Some((k, mods)) => key(1, k, mods),
            None => Parsed::Done(1, None),
        },
        0x20..=0x7e => key(1, Key::Char(char::from(b[0])), Mods::NONE),
        first => utf8(b, first),
    }
}

fn control(b: u8) -> Option<(Key, Mods)> {
    Some(match b {
        b'\r' | b'\n' => (Key::Enter, Mods::NONE),
        b'\t' => (Key::Tab, Mods::NONE),
        0x08 | 0x7f => (Key::Backspace, Mods::NONE),
        0x00 => (Key::Char(' '), Mods::CTRL),
        0x01..=0x1a => (Key::Char(char::from(b + 0x60)), Mods::CTRL),
        0x1c..=0x1f => (Key::Char(char::from(b + 0x40)), Mods::CTRL),
        _ => return None,
    })
}

fn utf8(b: &[u8], first: u8) -> Parsed {
    let len = match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return key(1, INVALID, Mods::NONE),
    };
    if b.len() < len {
        // Only continuation bytes can complete it; anything else means the
        // sequence is already broken.
        return if b[1..].iter().all(|&c| c & 0xc0 == 0x80) {
            Parsed::Incomplete
        } else {
            key(1, INVALID, Mods::NONE)
        };
    }
    match std::str::from_utf8(&b[..len])
        .ok()
        .and_then(|s| s.chars().next())
    {
        Some(c) => key(len, Key::Char(c), Mods::NONE),
        None => key(1, INVALID, Mods::NONE),
    }
}

fn escape(b: &[u8]) -> Parsed {
    let Some(&second) = b.get(1) else {
        return Parsed::Incomplete;
    };
    match second {
        b'[' => csi(b),
        b'O' => ss3(b),
        b']' | b'P' | b'_' | b'^' => string(b),
        // ST on its own: the end of a string whose start was dropped.
        b'\\' => Parsed::Done(2, None),
        // ESC ESC: the first is the Escape key on its own.
        0x1b => key(1, Key::Esc, Mods::NONE),
        _ => match parse(&b[1..]) {
            Parsed::Done(n, Some(Input::Key(k, mods))) => key(n + 1, k, mods.with(Mods::ALT)),
            Parsed::Done(n, other) => Parsed::Done(n + 1, other),
            Parsed::Paste(n) => Parsed::Paste(n + 1),
            Parsed::Incomplete => Parsed::Incomplete,
        },
    }
}

/// OSC, DCS, APC and PM strings: consumed through BEL or ST. Only the reply
/// to the background color query (OSC 11) means anything.
fn string(b: &[u8]) -> Parsed {
    for i in 2..b.len() {
        let (body, n) = match b[i] {
            0x07 => (&b[2..i], i + 1),
            0x1b if b.get(i + 1) == Some(&b'\\') => (&b[2..i], i + 2),
            0x1b if i + 1 < b.len() => return Parsed::Done(i, None),
            _ => continue,
        };
        let input = body
            .strip_prefix(b"11;")
            .filter(|_| b[1] == b']')
            .and_then(rgb)
            .map(|rgb| Input::Background { rgb });
        return Parsed::Done(n, input);
    }
    Parsed::Incomplete
}

/// The color of an X11 color spec (`rgb:RRRR/GGGG/BBBB`, 1 to 4 hex digits
/// per channel), 8 bits a channel.
fn rgb(spec: &[u8]) -> Option<(u8, u8, u8)> {
    let spec = std::str::from_utf8(spec.strip_prefix(b"rgb:")?).ok()?;
    let mut channels = spec.split('/').map(|c| {
        let max = 16u32.checked_pow(u32::try_from(c.len()).ok()?)? - 1;
        let value = u32::from(u16::from_str_radix(c, 16).ok()?);
        u8::try_from((value * 255 + max / 2) / max.max(1)).ok()
    });
    Some((channels.next()??, channels.next()??, channels.next()??))
}

fn ss3(b: &[u8]) -> Parsed {
    let Some(&c) = b.get(2) else {
        return Parsed::Incomplete;
    };
    let k = match c {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'P'..=b'S' => Key::F(c - b'P' + 1),
        _ => return Parsed::Done(3, None),
    };
    key(3, k, Mods::NONE)
}

fn csi(b: &[u8]) -> Parsed {
    match b.get(2) {
        None => return Parsed::Incomplete,
        Some(b'<') => return sgr_mouse(b),
        Some(b'M') => return x10_mouse(b),
        Some(_) => {}
    }
    let mut end = 2;
    while let Some(&c) = b.get(end) {
        match c {
            0x20..=0x3f => end += 1,
            0x40..=0x7e => break,
            // Not part of a CSI sequence: drop what came before it.
            _ => return Parsed::Done(end, None),
        }
    }
    let Some(&last) = b.get(end) else {
        return Parsed::Incomplete;
    };
    let mut params = [None; 4];
    for (slot, p) in params.iter_mut().zip(b[2..end].split(|&c| c == b';')) {
        *slot = std::str::from_utf8(p)
            .ok()
            .and_then(|s| s.parse::<u16>().ok());
    }
    let mods = params[1].map_or(Mods::NONE, |m| {
        Mods(u8::try_from(m.saturating_sub(1)).unwrap_or(0) & 7)
    });
    let k = match last {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'Z' => Key::BackTab,
        b'P'..=b'S' => Key::F(last - b'P' + 1),
        b'~' => match params[0] {
            Some(200) => return Parsed::Paste(end + 1),
            Some(1 | 7) => Key::Home,
            Some(4 | 8) => Key::End,
            Some(2) => Key::Insert,
            Some(3) => Key::Delete,
            Some(5) => Key::PageUp,
            Some(6) => Key::PageDown,
            Some(n @ 11..=15) => Key::F(u8::try_from(n - 10).unwrap_or(0)),
            Some(n @ 17..=21) => Key::F(u8::try_from(n - 11).unwrap_or(0)),
            Some(n @ 23..=24) => Key::F(u8::try_from(n - 12).unwrap_or(0)),
            _ => return Parsed::Done(end + 1, None),
        },
        _ => return Parsed::Done(end + 1, None),
    };
    key(end + 1, k, mods)
}

/// `ESC [ < button ; x ; y M` (press) or `m` (release), 1-based cells.
fn sgr_mouse(b: &[u8]) -> Parsed {
    let mut end = 3;
    loop {
        match b.get(end) {
            None => return Parsed::Incomplete,
            Some(b'0'..=b'9' | b';') => end += 1,
            Some(b'M' | b'm') => break,
            Some(_) => return Parsed::Done(end, None),
        }
    }
    let mut n = [0u16; 3];
    for (slot, p) in n.iter_mut().zip(b[3..end].split(|&c| c == b';')) {
        *slot = std::str::from_utf8(p)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
    }
    Parsed::Done(
        end + 1,
        mouse(
            n[0],
            n[1].saturating_sub(1),
            n[2].saturating_sub(1),
            b[end] == b'M',
        ),
    )
}

/// The legacy `ESC [ M b x y` report, from terminals without SGR mouse.
fn x10_mouse(b: &[u8]) -> Parsed {
    if b.len() < 6 {
        return Parsed::Incomplete;
    }
    let at = |i: usize| u16::from(b[i].saturating_sub(33));
    Parsed::Done(
        6,
        mouse(u16::from(b[3].saturating_sub(32)), at(4), at(5), true),
    )
}

fn mouse(button: u16, x: u16, y: u16, press: bool) -> Option<Input> {
    // Bit 6 marks wheel buttons, bit 5 motion; the low bits pick the button.
    // The legacy encoding reports any release as button 3.
    match (button & 0b1110_0011, press) {
        (64, true) => Some(Input::Scroll { delta: -1, x, y }),
        (65, true) => Some(Input::Scroll { delta: 1, x, y }),
        (0, true) => Some(Input::Click { x, y }),
        (32, true) => Some(Input::Drag { x, y }),
        (0, false) | (3, true) => Some(Input::Release { x, y }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(chunks: &[&[u8]]) -> Vec<Input> {
        let mut parser = Parser::default();
        let mut out = Vec::new();
        for chunk in chunks {
            parser.feed(chunk, &mut |i| out.push(i));
        }
        out
    }

    fn keys(chunks: &[&[u8]]) -> Vec<(Key, Mods)> {
        parse_all(chunks)
            .into_iter()
            .map(|i| match i {
                Input::Key(k, m) => (k, m),
                other => panic!("not a key: {other:?}"),
            })
            .collect()
    }

    const N: Mods = Mods::NONE;

    #[test]
    fn plain_bytes_controls_and_utf8() {
        assert_eq!(
            keys(&[b"jK\r\t\x7f\x04\x15 \xc3\xa9\xe2\x82\xac"]),
            [
                (Key::Char('j'), N),
                (Key::Char('K'), N),
                (Key::Enter, N),
                (Key::Tab, N),
                (Key::Backspace, N),
                (Key::Char('d'), Mods::CTRL),
                (Key::Char('u'), Mods::CTRL),
                (Key::Char(' '), N),
                (Key::Char('é'), N),
                (Key::Char('€'), N),
            ]
        );
    }

    #[test]
    fn cursor_and_editing_keys_with_modifiers() {
        assert_eq!(
            keys(&[b"\x1b[A\x1bOB\x1b[1;5C\x1b[5~\x1b[6~\x1b[H\x1b[4~\x1b[Z\x1b[1;2D\x1b[15~"]),
            [
                (Key::Up, N),
                (Key::Down, N),
                (Key::Right, Mods::CTRL),
                (Key::PageUp, N),
                (Key::PageDown, N),
                (Key::Home, N),
                (Key::End, N),
                (Key::BackTab, N),
                (Key::Left, Mods(1)),
                (Key::F(5), N),
            ]
        );
    }

    #[test]
    fn sequences_split_across_reads_are_joined() {
        assert_eq!(
            keys(&[b"\x1b", b"[", b"1;5", b"A", b"\xe2\x82", b"\xac"]),
            [(Key::Up, Mods::CTRL), (Key::Char('€'), N)]
        );
    }

    #[test]
    fn lone_escape_waits_for_the_timeout() {
        let mut parser = Parser::default();
        let mut out = Vec::new();
        parser.feed(b"\x1b", &mut |i| out.push(i));
        assert_eq!(out, []);
        assert!(parser.pending());
        parser.timeout(&mut |i| out.push(i));
        assert_eq!(out, [Input::Key(Key::Esc, N)]);
        assert!(!parser.pending());
        // After the timeout, a `[` is just a key, not the rest of a CSI.
        parser.feed(b"[", &mut |i| out.push(i));
        assert_eq!(out[1], Input::Key(Key::Char('['), N));
    }

    #[test]
    fn alt_is_escape_then_the_key() {
        assert_eq!(
            keys(&[b"\x1bj\x1b\x1b"]),
            [(Key::Char('j'), Mods::ALT), (Key::Esc, N)]
        );
    }

    #[test]
    fn mouse_wheel_and_click() {
        assert_eq!(
            parse_all(&[
                b"\x1b[<64;10;5M\x1b[<65;1;1M\x1b[<0;3;4M\x1b[<32;3;6M\x1b[<0;3;6m\x1b[<2;3;4M"
            ]),
            [
                Input::Scroll {
                    delta: -1,
                    x: 9,
                    y: 4
                },
                Input::Scroll {
                    delta: 1,
                    x: 0,
                    y: 0
                },
                Input::Click { x: 2, y: 3 },
                Input::Drag { x: 2, y: 5 },
                Input::Release { x: 2, y: 5 },
            ]
        );
        assert_eq!(
            parse_all(&[b"\x1b[M`!!"]),
            [Input::Scroll {
                delta: -1,
                x: 0,
                y: 0
            }]
        );
    }

    /// A paste's end with no start, as after one cut short, is swallowed
    /// like any unknown sequence.
    #[test]
    fn unknown_sequences_are_swallowed_whole() {
        assert_eq!(
            keys(&[b"\x1b[?1;2c\x1b]2;title\x07\x1b[201~x\x1b[99zq"]),
            [(Key::Char('x'), N), (Key::Char('q'), N)]
        );
    }

    /// A bracketed paste arrives as one input whatever its length and
    /// however the reads split it, its terminator included; escapes and
    /// line ends inside it are text, and the escape timeout never fires
    /// in the middle of one.
    #[test]
    fn a_bracketed_paste_is_one_input() {
        let mut parser = Parser::default();
        let mut out = Vec::new();
        let long = "y".repeat(500);
        for chunk in [
            &b"a\x1b[200~li"[..],
            b"ne\r\n\x1b[Ax",
            long.as_bytes(),
            b"\x1b[20",
        ] {
            parser.feed(chunk, &mut |i| out.push(i));
            assert!(!parser.pending(), "no timeout mid-paste");
        }
        parser.timeout(&mut |i| out.push(i));
        parser.feed(b"1~b", &mut |i| out.push(i));
        assert_eq!(
            out,
            [
                Input::Key(Key::Char('a'), N),
                Input::Paste(format!("line\r\n\x1b[Ax{long}")),
                Input::Key(Key::Char('b'), N),
            ]
        );
    }

    #[test]
    fn background_color_replies_are_read_whole_and_never_keys() {
        assert_eq!(
            parse_all(&[b"\x1b]11;rgb:ff", b"ff/fafa/f0", b"f0\x1b", b"\\j"]),
            [
                Input::Background {
                    rgb: (0xff, 0xfa, 0xf0)
                },
                Input::Key(Key::Char('j'), N)
            ]
        );
        assert_eq!(
            parse_all(&[b"\x1b]11;rgb:1e/1e/2e\x07"]),
            [Input::Background {
                rgb: (0x1e, 0x1e, 0x2e)
            }]
        );
        assert_eq!(
            parse_all(&[b"\x1b]11;rgb:0/f/0\x07"]),
            [Input::Background { rgb: (0, 255, 0) }]
        );
    }

    /// A reply slow enough that the ESC timeout fires halfway through is
    /// lost, but its rest never turns into keys, whichever terminator ends
    /// it and however that splits.
    #[test]
    fn a_reply_cut_by_the_escape_timeout_is_dropped_not_typed() {
        for rest in [
            &[&b"1e1e/2e2e\x1b\\j"[..]][..],
            &[b"1e1e/2e2e\x1b", b"\\j"],
            &[b"2e\x07j"],
        ] {
            let mut parser = Parser::default();
            let mut out = Vec::new();
            parser.feed(b"\x1b]11;rgb:1e1e/", &mut |i| out.push(i));
            assert!(parser.pending());
            parser.timeout(&mut |i| out.push(i));
            for bytes in rest {
                parser.feed(bytes, &mut |i| out.push(i));
            }
            assert_eq!(out, [Input::Key(Key::Char('j'), N)], "{rest:?}");
        }
    }

    /// A timeout right after `ESC ]` isn't Alt-]: the reply's rest, when it
    /// comes, completes it.
    #[test]
    fn a_reply_cut_after_its_introducer_waits_for_the_rest() {
        let mut parser = Parser::default();
        let mut out = Vec::new();
        parser.feed(b"\x1b]", &mut |i| out.push(i));
        assert!(parser.pending());
        parser.timeout(&mut |i| out.push(i));
        assert_eq!(out, []);
        assert!(!parser.pending(), "waits with no timeout");
        parser.feed(b"11;rgb:1e/1e/2e\x07j", &mut |i| out.push(i));
        assert_eq!(
            out,
            [
                Input::Background {
                    rgb: (0x1e, 0x1e, 0x2e)
                },
                Input::Key(Key::Char('j'), N)
            ]
        );
    }
}
