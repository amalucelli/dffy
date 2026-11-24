//! How many columns text takes when dffy draws it. The same rule as dffy's
//! own: a row it's sent is placed by that one, so where the two drift,
//! what a row has after the text is out of line with the rows around it.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// U+FE0F, asking for the emoji form of the character before it.
const SELECTOR: char = '\u{fe0f}';

/// The columns `text` takes.
#[must_use]
pub fn width(text: &str) -> usize {
    clusters(text).map(|(_, width)| width).sum()
}

/// The pieces `text` can be cut between, with the columns of each: a
/// character, with its U+FE0F when that makes it two columns wide.
pub fn clusters(text: &str) -> impl Iterator<Item = (&str, usize)> {
    let mut rest = text;
    std::iter::from_fn(move || {
        let c = rest.chars().next()?;
        let mut end = c.len_utf8();
        let mut width = UnicodeWidthChar::width(c).unwrap_or(0);
        let selected = end + SELECTOR.len_utf8();
        if width == 1
            && rest[end..].starts_with(SELECTOR)
            && UnicodeWidthStr::width(&rest[..selected]) == 2
        {
            (end, width) = (selected, 2);
        }
        let (piece, after) = rest.split_at(end);
        rest = after;
        Some((piece, width))
    })
}
