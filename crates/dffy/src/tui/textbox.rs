//! A multi-line text input drawn inside the diff, for writing comments:
//! typing, pasting, backspace and delete, and the cursor moving by
//! character, by wrapped row and to a row's ends. Rows wrap at spaces to
//! the width they were last drawn at, which is what up and down move by.

use std::ops::Range;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::term::{Key, Mods};

#[derive(Debug, Default)]
pub(crate) struct TextBox {
    text: String,
    /// A byte offset into `text`, on a character boundary.
    cursor: usize,
    /// Columns the rows wrapped at when last drawn; 0 before.
    width: usize,
}

impl TextBox {
    /// A box holding `text`, the cursor at its end.
    pub(crate) fn new(text: &str) -> Self {
        let mut boxed = Self::default();
        boxed.paste(text);
        boxed
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Edits for `key`: characters, `enter` (a new line), `tab` (four
    /// spaces), backspace, delete, the arrows, home and end. Keys with
    /// ctrl or alt do nothing.
    pub(crate) fn key(&mut self, key: Key, mods: Mods) {
        if mods != Mods::NONE {
            return;
        }
        match key {
            Key::Char(c) if !c.is_control() => self.insert(c.encode_utf8(&mut [0; 4])),
            Key::Enter => self.insert("\n"),
            Key::Tab => self.insert("    "),
            Key::Backspace => {
                if let Some(c) = self.text[..self.cursor].chars().next_back() {
                    self.cursor -= c.len_utf8();
                    self.text.remove(self.cursor);
                }
            }
            Key::Delete if self.cursor < self.text.len() => {
                self.text.remove(self.cursor);
            }
            Key::Left => {
                if let Some(c) = self.text[..self.cursor].chars().next_back() {
                    self.cursor -= c.len_utf8();
                }
            }
            Key::Right => {
                if let Some(c) = self.text[self.cursor..].chars().next() {
                    self.cursor += c.len_utf8();
                }
            }
            Key::Up | Key::Down => self.vertical(key == Key::Down),
            Key::Home | Key::End => {
                let rows = self.ranges(self.width);
                let (row, _) = locate(&self.text, &rows, self.cursor);
                let range = &rows[row];
                let end = trailing(&self.text, range);
                self.cursor = if key == Key::Home {
                    range.start
                } else if end == range.end && row + 1 < rows.len() {
                    // A wrapped row's end is the next row's start: the
                    // cursor stops before its last character instead.
                    self.text[..end]
                        .char_indices()
                        .next_back()
                        .map_or(end, |(i, _)| i.max(range.start))
                } else {
                    end
                };
            }
            _ => {}
        }
    }

    /// Inserts `text` at the cursor: line ends become `\n`, tabs four
    /// spaces, and other control characters are dropped.
    pub(crate) fn paste(&mut self, text: &str) {
        let mut clean = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' => {
                    chars.next_if_eq(&'\n');
                    clean.push('\n');
                }
                '\n' => clean.push('\n'),
                '\t' => clean.push_str("    "),
                c if c.is_control() => {}
                c => clean.push(c),
            }
        }
        self.insert(&clean);
    }

    fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Moves the cursor a row up or down, to the column nearest the one it
    /// was at.
    fn vertical(&mut self, down: bool) {
        let rows = self.ranges(self.width);
        let (row, column) = locate(&self.text, &rows, self.cursor);
        let target = if down {
            row + 1
        } else {
            let Some(up) = row.checked_sub(1) else {
                self.cursor = 0;
                return;
            };
            up
        };
        let Some(range) = rows.get(target) else {
            self.cursor = self.text.len();
            return;
        };
        let mut at = range.start;
        let mut used = 0;
        for (i, c) in self.text[range.start..trailing(&self.text, range)].char_indices() {
            let w = c.width().unwrap_or(0);
            if used + w > column {
                break;
            }
            used += w;
            at = range.start + i + c.len_utf8();
        }
        self.cursor = at;
    }

    /// The rows `width` columns wide, each with the cursor's column when
    /// it's on that row.
    pub(crate) fn rows(&mut self, width: usize) -> Vec<(String, Option<usize>)> {
        self.width = width;
        let rows = self.ranges(width);
        let (at, column) = locate(&self.text, &rows, self.cursor);
        rows.iter()
            .enumerate()
            .map(|(i, r)| {
                let text = self.text[r.start..trailing(&self.text, r)].to_owned();
                let cursor = (i == at).then(|| {
                    let before = &self.text[r.start..self.cursor.max(r.start)];
                    debug_assert_eq!(before.width(), column);
                    before.len()
                });
                (text, cursor)
            })
            .collect()
    }

    /// The text's rows as byte ranges, each line wrapped at spaces to
    /// `width` columns (0: not wrapped), a word longer than that broken at
    /// the edge. A line's `\n` ends its last row's range.
    fn ranges(&self, width: usize) -> Vec<Range<usize>> {
        let width = if width == 0 { usize::MAX } else { width };
        let mut rows = Vec::new();
        let mut start = 0;
        for line in self.text.split_inclusive('\n').chain(
            // An empty last line, after a final `\n` or in an empty text.
            (self.text.is_empty() || self.text.ends_with('\n')).then_some(""),
        ) {
            let base = start;
            let mut row = base;
            let mut used = 0;
            let mut space = None;
            for (i, c) in line.char_indices() {
                let at = base + i;
                if c == '\n' {
                    break;
                }
                let w = c.width().unwrap_or(0);
                if used + w > width && at > row {
                    let cut = space.filter(|&s| s > row).unwrap_or(at);
                    rows.push(row..cut);
                    used = self.text[cut..at].width();
                    row = cut;
                    space = None;
                }
                used += w;
                if c == ' ' {
                    space = Some(at + 1);
                }
            }
            start = base + line.len();
            rows.push(row..start);
        }
        rows
    }
}

/// The text of `range` without its line's `\n`.
fn trailing(text: &str, range: &Range<usize>) -> usize {
    if text[range.clone()].ends_with('\n') {
        range.end - 1
    } else {
        range.end
    }
}

/// The row holding byte `cursor` and its column there. A cursor where one
/// wrapped row ends and the next starts is on the next.
fn locate(text: &str, rows: &[Range<usize>], cursor: usize) -> (usize, usize) {
    let row = rows
        .iter()
        .position(|r| cursor < r.end)
        .unwrap_or_else(|| rows.len().saturating_sub(1));
    let start = rows.get(row).map_or(0, |r| r.start);
    (row, text[start..cursor].width())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(keys: &str) -> TextBox {
        let mut b = TextBox::default();
        for c in keys.chars() {
            b.key(Key::Char(c), Mods::NONE);
        }
        b
    }

    fn shown(b: &mut TextBox, width: usize) -> Vec<String> {
        b.rows(width)
            .into_iter()
            .map(|(text, cursor)| match cursor {
                Some(at) => format!("{}|{}", &text[..at], &text[at..]),
                None => text,
            })
            .collect()
    }

    /// Typing goes in at the cursor, `enter` starts a line, backspace and
    /// delete take a character either side, and rows wrap at spaces.
    #[test]
    fn typing_wraps_and_edits_at_the_cursor() {
        let mut b = typed("one two three");
        assert_eq!(shown(&mut b, 8), ["one two ", "three|"]);
        b.key(Key::Enter, Mods::NONE);
        b.key(Key::Char('x'), Mods::NONE);
        assert_eq!(b.text(), "one two three\nx");
        for key in [Key::Left, Key::Left, Key::Backspace] {
            b.key(key, Mods::NONE);
        }
        assert_eq!(b.text(), "one two thre\nx");
        b.key(Key::Delete, Mods::NONE);
        assert_eq!(shown(&mut b, 8), ["one two ", "thre|x"]);
        b.key(Key::Char('e'), Mods::CTRL);
        assert_eq!(b.text(), "one two threx", "ctrl keys don't type");
    }

    /// Up and down go by wrapped rows, keeping the column; home and end go
    /// to the row's ends.
    #[test]
    fn the_cursor_moves_by_rows() {
        let mut b = TextBox::new("abcdefghij\nxy");
        shown(&mut b, 4);
        b.key(Key::Up, Mods::NONE);
        assert_eq!(shown(&mut b, 4), ["abcd", "efgh", "ij|", "xy"]);
        b.key(Key::Up, Mods::NONE);
        assert_eq!(shown(&mut b, 4), ["abcd", "ef|gh", "ij", "xy"]);
        b.key(Key::End, Mods::NONE);
        b.key(Key::Down, Mods::NONE);
        assert_eq!(shown(&mut b, 4), ["abcd", "efgh", "ij|", "xy"]);
        b.key(Key::Home, Mods::NONE);
        b.key(Key::Up, Mods::NONE);
        b.key(Key::Up, Mods::NONE);
        b.key(Key::Up, Mods::NONE);
        assert_eq!(shown(&mut b, 4)[0], "|abcd");
    }

    /// A paste goes in whole at the cursor, CRLF and CR as line ends, tabs
    /// as spaces, other controls dropped; an empty box is one empty row.
    #[test]
    fn a_paste_goes_in_at_the_cursor() {
        let mut b = TextBox::default();
        assert_eq!(shown(&mut b, 10), ["|"]);
        b.paste("a\r\nb\rc\td\u{1b}e\n");
        assert_eq!(b.text(), "a\nb\nc    de\n");
        assert_eq!(shown(&mut b, 10), ["a", "b", "c    de", "|"]);
    }
}
