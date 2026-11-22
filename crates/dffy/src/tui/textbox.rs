//! A multi-line text input drawn inside the diff, for writing comments.
//! Rows wrap at spaces to the width they were last drawn at, which is what
//! up and down move by.

use std::ops::Range;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::term::{Key, Mods};

#[derive(Debug, Default, Clone)]
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

    /// Puts the cursor before the text.
    pub(crate) fn rewind(&mut self) {
        self.cursor = 0;
    }

    /// Edits for `key`. Moves and deletes by word and to the line's start
    /// take readline's keys, which macOS terminals send for option and
    /// command. Enter starts a line, and so do shift+enter, alt+enter and
    /// ctrl+j, for an owner that takes enter alone to send the text: only
    /// terminals with the kitty keyboard protocol tell shift+enter from
    /// enter, and every terminal sends the other two. Other keys with ctrl
    /// or alt do nothing.
    pub(crate) fn key(&mut self, key: Key, mods: Mods) {
        match (key, mods) {
            (Key::Enter, Mods::SHIFT | Mods::ALT) | (Key::Char('j'), Mods::CTRL) => {
                self.insert("\n");
            }
            (Key::Backspace, Mods::ALT) | (Key::Char('w'), Mods::CTRL) => {
                self.cut(word_start(&self.text, self.cursor)..self.cursor);
            }
            (Key::Delete | Key::Char('d'), Mods::ALT) => {
                self.cut(self.cursor..word_end(&self.text, self.cursor));
            }
            (Key::Left | Key::Char('b'), Mods::ALT) => {
                self.cursor = word_start(&self.text, self.cursor);
            }
            (Key::Right | Key::Char('f'), Mods::ALT) => {
                self.cursor = word_end(&self.text, self.cursor);
            }
            (Key::Char('u'), Mods::CTRL) => {
                let start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
                self.cut(start..self.cursor);
            }
            (Key::Char('a'), Mods::CTRL) => self.plain(Key::Home),
            (Key::Char('e'), Mods::CTRL) => self.plain(Key::End),
            (key, Mods::NONE) => self.plain(key),
            _ => {}
        }
    }

    fn plain(&mut self, key: Key) {
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

    /// Deletes `range`, which holds the cursor at one end.
    fn cut(&mut self, range: Range<usize>) {
        self.cursor = range.start;
        self.text.replace_range(range, "");
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
                    let before = &self.text[r.start..self.cursor];
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

/// Where `range` ends, before its line's `\n`.
fn trailing(text: &str, range: &Range<usize>) -> usize {
    if text[range.clone()].ends_with('\n') {
        range.end - 1
    } else {
        range.end
    }
}

/// Words are runs of letters and digits, as in readline. Going back or
/// ahead by one passes the spaces and punctuation next to the cursor, then
/// a word, never into another line; a line break next to the cursor is a
/// step of its own.
fn word(c: char) -> bool {
    c.is_alphanumeric()
}

fn word_start(text: &str, cursor: usize) -> usize {
    let before = &text[..cursor];
    if before.ends_with('\n') {
        return cursor - 1;
    }
    let line = &before[before.rfind('\n').map_or(0, |i| i + 1)..];
    let rest = line.trim_end_matches(|c| !word(c)).trim_end_matches(word);
    cursor - line.len() + rest.len()
}

fn word_end(text: &str, cursor: usize) -> usize {
    let after = &text[cursor..];
    if after.starts_with('\n') {
        return cursor + 1;
    }
    let line = &after[..after.find('\n').unwrap_or(after.len())];
    let rest = line
        .trim_start_matches(|c| !word(c))
        .trim_start_matches(word);
    cursor + line.len() - rest.len()
}

/// The row holding byte `cursor` and its column there. A cursor where one
/// wrapped row ends and the next starts is on the next.
fn locate(text: &str, rows: &[Range<usize>], cursor: usize) -> (usize, usize) {
    let row = rows
        .iter()
        .position(|r| cursor < r.end)
        .unwrap_or(rows.len() - 1);
    (row, text[rows[row].start..cursor].width())
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

    /// `enter` starts a line, and backspace and delete take a character
    /// either side.
    #[test]
    fn typing_wraps_and_edits_at_the_cursor() {
        let mut b = typed("one two threes");
        assert_eq!(shown(&mut b, 8), ["one two ", "threes|"]);
        b.key(Key::Enter, Mods::NONE);
        b.key(Key::Char('x'), Mods::NONE);
        assert_eq!(b.text(), "one two threes\nx");
        for key in [Key::Left, Key::Left, Key::Backspace] {
            b.key(key, Mods::NONE);
        }
        assert_eq!(b.text(), "one two three\nx");
        b.key(Key::Delete, Mods::NONE);
        assert_eq!(shown(&mut b, 8), ["one two ", "three|x"]);
        b.key(Key::Char('x'), Mods::CTRL);
        b.key(Key::Char('x'), Mods::ALT);
        assert_eq!(b.text(), "one two threex", "ctrl and alt keys don't type");
    }

    /// Shift+enter, alt+enter and ctrl+j start a line as enter does, for
    /// an owner that takes enter to send.
    #[test]
    fn new_line_keys() {
        let mut b = typed("a");
        for (key, mods) in [
            (Key::Enter, Mods::SHIFT),
            (Key::Enter, Mods::ALT),
            (Key::Char('j'), Mods::CTRL),
        ] {
            b.key(key, mods);
        }
        assert_eq!(b.text(), "a\n\n\n");
    }

    fn at(text: &str, cursor: usize) -> TextBox {
        let mut b = TextBox::new(text);
        b.cursor = cursor;
        b
    }

    fn marked(b: &TextBox) -> String {
        format!("{}|{}", &b.text[..b.cursor], &b.text[b.cursor..])
    }

    /// Alt+backspace and ctrl+w take the word before the cursor, or its
    /// part before the cursor, with the spaces and punctuation after it;
    /// at a line's start they take only the line break.
    #[test]
    fn a_word_goes_back_to_its_start() {
        let mut b = at("one, twofold", 9);
        b.key(Key::Backspace, Mods::ALT);
        assert_eq!(marked(&b), "one, |old");
        b.key(Key::Char('w'), Mods::CTRL);
        assert_eq!(marked(&b), "|old");
        let mut b = at("one two\nthree", 8);
        b.key(Key::Backspace, Mods::ALT);
        assert_eq!(marked(&b), "one two|three");
        let mut b = at("x\n  ", 4);
        b.key(Key::Backspace, Mods::ALT);
        assert_eq!(marked(&b), "x\n|", "stops at the line's start");
        let mut b = TextBox::new("naïve café");
        b.key(Key::Backspace, Mods::ALT);
        assert_eq!(marked(&b), "naïve |");
    }

    /// Alt+d and alt+delete take the word after the cursor; alt with the
    /// arrows, `b` or `f` moves past one, a line break a step of its own.
    #[test]
    fn words_ahead_and_moving_by_word() {
        let mut b = at("one, two three", 3);
        b.key(Key::Char('d'), Mods::ALT);
        assert_eq!(marked(&b), "one| three");
        b.key(Key::Delete, Mods::ALT);
        assert_eq!(marked(&b), "one|");
        let mut b = at("ab cd\nef", 0);
        for (key, want) in [
            (Key::Right, "ab| cd\nef"),
            (Key::Char('f'), "ab cd|\nef"),
            (Key::Right, "ab cd\n|ef"),
            (Key::Left, "ab cd|\nef"),
            (Key::Char('b'), "ab |cd\nef"),
        ] {
            b.key(key, Mods::ALT);
            assert_eq!(marked(&b), want);
        }
    }

    /// Ctrl+u clears past the rows the line wraps into; ctrl+a goes to the
    /// row's start, like home, and ctrl+e to its end, like end.
    #[test]
    fn ctrl_u_clears_back_to_the_line_start() {
        let mut b = TextBox::new("top\none two three four");
        assert_eq!(shown(&mut b, 8), ["top", "one two ", "three ", "four|"]);
        b.key(Key::Char('a'), Mods::CTRL);
        assert_eq!(shown(&mut b, 8), ["top", "one two ", "three ", "|four"]);
        b.key(Key::Right, Mods::NONE);
        b.key(Key::Char('u'), Mods::CTRL);
        assert_eq!(shown(&mut b, 8), ["top", "|our"]);
        b.key(Key::Char('u'), Mods::CTRL);
        assert_eq!(b.text(), "top\nour", "nothing before it on its line");
        b.key(Key::Char('e'), Mods::CTRL);
        assert_eq!(shown(&mut b, 8), ["top", "our|"]);
    }

    /// Up and down keep the column; home and end go to the wrapped row's
    /// ends.
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

    /// CRLF and CR become line ends, tabs spaces, and other controls are
    /// dropped; an empty box is one empty row.
    #[test]
    fn a_paste_goes_in_at_the_cursor() {
        let mut b = TextBox::default();
        assert_eq!(shown(&mut b, 10), ["|"]);
        b.paste("a\r\nb\rc\td\u{1b}e\n");
        assert_eq!(b.text(), "a\nb\nc    de\n");
        assert_eq!(shown(&mut b, 10), ["a", "b", "c    de", "|"]);
    }
}
