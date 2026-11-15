//! Fzf-style fuzzy matching for the file picker and the viewer's list
//! filters. The query's characters appear in the text in order, case
//! ignored; contiguous runs and starts of words and path components score
//! higher.

/// Scores `text` by how well `query` (lowercase) matches it, with the byte
/// offsets of the matched characters, or `None` when it doesn't. A match
/// after the last `/` (a path's file name) scores higher.
pub(super) fn score(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let lower = text.to_ascii_lowercase();
    let base = lower.rfind('/').map_or(0, |i| i + 1);
    let shorter = -i64::try_from(text.len() / 4).unwrap_or(0);
    if let Some((s, positions)) = best(query.as_bytes(), lower.as_bytes(), base) {
        return Some((s + 100 + shorter, positions));
    }
    best(query.as_bytes(), lower.as_bytes(), 0).map(|(s, p)| (s + shorter, p))
}

/// [`score`] for labels and descriptions, where a query's letters strewn
/// across a sentence aren't a match: they must fall within three times
/// the query's length.
pub(super) fn score_close(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
    let (s, at) = score(query, text)?;
    match (at.first(), at.last()) {
        (Some(first), Some(last)) if last - first >= query.len() * 3 => None,
        _ => Some((s, at)),
    }
}

/// The best alignment of `query` in `text[from..]`: each place the first
/// character occurs is tried, the rest matched as early as possible.
fn best(query: &[u8], text: &[u8], from: usize) -> Option<(i64, Vec<usize>)> {
    let mut found: Option<(i64, Vec<usize>)> = None;
    for start in (from..text.len()).filter(|&i| text[i] == query[0]) {
        let mut positions = vec![start];
        let mut at = start + 1;
        for &q in &query[1..] {
            let Some(i) = text[at..].iter().position(|&b| b == q) else {
                break;
            };
            positions.push(at + i);
            at += i + 1;
        }
        if positions.len() < query.len() {
            // Later starts leave even less text.
            break;
        }
        let mut s = 0;
        for (k, &p) in positions.iter().enumerate() {
            s += 16;
            let boundary = p == 0 || matches!(text[p - 1], b'/' | b'_' | b'-' | b'.' | b' ');
            if boundary {
                s += 12;
            }
            if k > 0 {
                let gap = p - positions[k - 1] - 1;
                if gap == 0 {
                    s += 24;
                } else {
                    s -= i64::try_from(gap.min(8)).unwrap_or(8);
                }
            }
        }
        if found.as_ref().is_none_or(|(f, _)| s > *f) {
            found = Some((s, positions));
        }
    }
    found
}

/// Writes `text` from column `x` of row `y`, the characters at byte
/// offsets `matched` in `hit` and the rest in `style`. Returns the column
/// after it.
pub(super) fn put_marked(
    grid: &mut crate::term::Grid,
    (mut x, y, end): (usize, usize, usize),
    text: &str,
    matched: &[usize],
    (style, hit): (crate::theme::Style, crate::theme::Style),
) -> usize {
    for (i, c) in text.char_indices() {
        let s = if matched.contains(&i) { hit } else { style };
        x = grid.put_char(x, y, end, c, s);
    }
    x
}
