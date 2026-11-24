//! How many columns text takes on the terminal. One rule for everything that
//! places text, so what's measured, what the grid holds and what a click
//! maps to are the same cells: each character takes its own width, and
//! [`SELECTOR`] one column more after a character it makes an emoji.
//!
//! Sequences that terminals clustering graphemes draw as one emoji (joined
//! by U+200D, or with a skin tone) are measured as the characters they're
//! made of. Terminals disagree on those, and everything here places the
//! characters one by one.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// U+FE0F, asking for the emoji form of the character before it.
pub(crate) const SELECTOR: char = '\u{fe0f}';

/// Whether [`SELECTOR`] after `c` makes it two columns wide.
#[cold]
pub(crate) fn selectable(c: char) -> bool {
    let mut buf = [0; 8];
    let len = c.encode_utf8(&mut buf).len();
    let end = len + SELECTOR.encode_utf8(&mut buf[len..]).len();
    UnicodeWidthChar::width(c) == Some(1)
        && std::str::from_utf8(&buf[..end]).is_ok_and(|s| UnicodeWidthStr::width(s) == 2)
}

/// The columns `c` adds to text that ends with `before`.
#[inline]
pub(crate) fn added(before: Option<char>, c: char) -> usize {
    if c == SELECTOR && before.is_some_and(selectable) {
        return 1;
    }
    UnicodeWidthChar::width(c).unwrap_or(0)
}

#[inline(never)]
fn width_by_character(text: &str) -> usize {
    let mut before = None;
    let mut width = 0;
    for c in text.chars() {
        width += if matches!(c, ' '..='~') {
            1
        } else {
            added(before, c)
        };
        before = Some(c);
    }
    width
}

pub(crate) trait Columns {
    /// The columns it takes.
    fn width(&self) -> usize;

    /// The pieces it can be cut between, each with where it starts and its
    /// columns: a character, with its [`SELECTOR`] when that widens it.
    fn clusters(&self) -> impl Iterator<Item = (usize, &str, usize)>;
}

impl Columns for str {
    // Inlined up to the rest: the renderer measures each glyph it maps.
    #[inline]
    fn width(&self) -> usize {
        // Printable ASCII takes a column a byte; nearly all text measured is
        // that, a glyph or a few at a time.
        if self.bytes().all(|b| matches!(b, b' '..=b'~')) {
            return self.len();
        }
        width_by_character(self)
    }

    fn clusters(&self) -> impl Iterator<Item = (usize, &Self, usize)> {
        let mut at = 0;
        std::iter::from_fn(move || {
            let start = at;
            let rest = &self[start..];
            let c = rest.chars().next()?;
            at += c.len_utf8();
            let mut width = UnicodeWidthChar::width(c).unwrap_or(0);
            if rest[c.len_utf8()..].starts_with(SELECTOR) && selectable(c) {
                at += SELECTOR.len_utf8();
                width += 1;
            }
            Some((start, &self[start..at], width))
        })
    }
}

/// Text whose measures have disagreed: emoji by selector (twice, on an emoji
/// already wide, on an ASCII keycap, after a combining mark, on nothing),
/// joined sequences, flags, skin tones, the text form, marks.
#[cfg(test)]
pub(crate) const TRICKY: [&str; 16] = [
    "\u{26a0}\u{fe0f}",
    "1\u{fe0f}\u{20e3}",
    "\u{26a0}\u{fe0f}\u{fe0f}",
    "\u{2705}\u{fe0f}",
    "\u{26a0}\u{301}\u{fe0f}",
    "\u{fe0f}",
    "x\u{fe0f}",
    "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
    "\u{2764}\u{fe0f}\u{200d}\u{1f525}",
    "\u{1f3f3}\u{fe0f}\u{200d}\u{1f308}",
    "\u{1f1e7}\u{1f1f7}",
    "\u{1f44d}\u{1f3fd}",
    "\u{270c}\u{1f3fd}",
    "\u{270c}\u{fe0f}",
    "\u{231a}\u{fe0e}",
    "e\u{301}",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Clusters are the text, whole, and add up to its width.
    #[test]
    fn clusters_are_the_text_and_add_up_to_its_width() {
        for s in TRICKY {
            let text = format!("a{s}b");
            let joined: String = text.clusters().map(|(_, piece, _)| piece).collect();
            assert_eq!(joined, text);
            let total: usize = text.clusters().map(|(.., w)| w).sum();
            assert_eq!(total, Columns::width(text.as_str()), "{text:?}");
        }
    }

    /// What's measured is what the grid takes and where an emulator that
    /// draws each character by itself puts what follows.
    #[test]
    fn the_grid_and_an_emulator_take_the_columns_measured() {
        let size = |n: u16| std::num::NonZero::new(n).expect("non-zero");
        for s in TRICKY {
            let text = format!("a{s}b");
            let width = Columns::width(text.as_str());
            let mut grid = crate::term::Grid::new(20, 1);
            let after = grid.put_str(0, 0, 20, &text, crate::theme::Style::PLAIN);
            assert_eq!(after, width, "on the grid: {text:?}");
            assert_eq!(grid.row(0)[width - 1].text(), "b", "{text:?}");
            let mut vt = vt100::Parser::new(size(1), size(20), 0);
            vt.process(text.as_bytes());
            let at = usize::from(vt.screen().cursor_position().1);
            assert_eq!(at, width, "on the emulator: {text:?}");
        }
    }
}
