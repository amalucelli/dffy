//! Fzf-style fuzzy matching for the file picker and the lists' filters
//! and searches. The query's characters appear in the text in order, case
//! ignored; contiguous runs and starts of words and path components score
//! higher.

/// Scores `text` by how well `query` (lowercase) matches it, with the byte
/// offsets of the matched characters, or `None` when it doesn't. A match
/// after the last `/` (a path's file name) scores higher.
pub(crate) fn score(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
    let lower = text.to_ascii_lowercase();
    let mut positions = Vec::new();
    let s = matched(query, &lower, Some(&mut positions))?;
    Some((s, positions))
}

/// [`score`]'s score alone, for a text already lowercased (ASCII only, so
/// byte offsets stay those of the text). It allocates nothing, for lists
/// scored whole on each keystroke.
pub(crate) fn rank(query: &str, lower: &str) -> Option<i64> {
    matched(query, lower, None)
}

fn matched(query: &str, lower: &str, mut positions: Option<&mut Vec<usize>>) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let base = lower.rfind('/').map_or(0, |i| i + 1);
    let shorter = -i64::try_from(lower.len() / 4).unwrap_or(0);
    let (query, text) = (query.as_bytes(), lower.as_bytes());
    if let Some(s) = best(query, text, base, positions.as_deref_mut()) {
        return Some(s + 100 + shorter);
    }
    best(query, text, 0, positions).map(|s| s + shorter)
}

/// [`score`] for labels and descriptions, where a query's letters strewn
/// across a sentence aren't a match: they must fall within three times
/// the query's length.
pub(crate) fn score_close(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
    let (s, at) = score(query, text)?;
    match (at.first(), at.last()) {
        (Some(first), Some(last)) if last - first >= query.len() * 3 => None,
        _ => Some((s, at)),
    }
}

/// The best alignment of `query` in `text[from..]`: each place the first
/// character occurs is tried, the rest matched as early as possible. With
/// `positions`, the best alignment's offsets go there.
fn best(query: &[u8], text: &[u8], from: usize, positions: Option<&mut Vec<usize>>) -> Option<i64> {
    let mut found: Option<(i64, usize)> = None;
    for start in (from..text.len()).filter(|&i| text[i] == query[0]) {
        let Some(s) = aligned(query, text, start, None) else {
            // Later starts leave even less text.
            break;
        };
        if found.is_none_or(|(f, _)| s > f) {
            found = Some((s, start));
        }
    }
    let (s, start) = found?;
    if let Some(positions) = positions {
        positions.clear();
        aligned(query, text, start, Some(positions));
    }
    Some(s)
}

/// The score of `query` matched from `start` on, each later character as
/// early as possible; `None` when it runs out of text.
fn aligned(
    query: &[u8],
    text: &[u8],
    start: usize,
    mut positions: Option<&mut Vec<usize>>,
) -> Option<i64> {
    let mut s = 0;
    let mut previous: Option<usize> = None;
    let mut at = start;
    for (k, &q) in query.iter().enumerate() {
        let p = if k == 0 {
            start
        } else {
            at + text[at..].iter().position(|&b| b == q)?
        };
        at = p + 1;
        if let Some(positions) = positions.as_deref_mut() {
            positions.push(p);
        }
        s += 16;
        if p == 0 || matches!(text[p - 1], b'/' | b'_' | b'-' | b'.' | b' ') {
            s += 12;
        }
        if let Some(previous) = previous {
            let gap = p - previous - 1;
            if gap == 0 {
                s += 24;
            } else {
                s -= i64::try_from(gap.min(8)).unwrap_or(8);
            }
        }
        previous = Some(p);
    }
    Some(s)
}
