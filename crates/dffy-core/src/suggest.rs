//! "Did you mean" matching for revision names and config keys.

/// The candidate closest to `name`, if it is within a third of the name's
/// length in edits (at least 2).
pub fn closest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (name.chars().count() / 3).max(2);
    candidates
        .into_iter()
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .filter(|(d, _)| *d <= limit)
        .min()
        .map(|(_, candidate)| candidate)
}

/// Optimal string alignment distance: Levenshtein plus adjacent
/// transpositions, so `amin` is one edit from `main`.
#[must_use]
pub fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let width = b.len() + 1;
    let mut d = vec![0; (a.len() + 1) * width];
    for (j, cell) in d.iter_mut().enumerate().take(width) {
        *cell = j;
    }
    for i in 1..=a.len() {
        d[i * width] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[(i - 1) * width + j] + 1)
                .min(d[i * width + j - 1] + 1)
                .min(d[(i - 1) * width + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[(i - 2) * width + j - 2] + 1);
            }
            d[i * width + j] = best;
        }
    }
    d[a.len() * width + b.len()]
}

#[cfg(test)]
mod tests {
    use super::{closest, edit_distance};

    #[test]
    fn edit_distance_counts_transpositions_once() {
        assert_eq!(edit_distance("main", "main"), 0);
        assert_eq!(edit_distance("amin", "main"), 1);
        assert_eq!(edit_distance("v6.2", "v6.1"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    #[test]
    fn closest_respects_the_distance_limit() {
        assert_eq!(closest("tehme", ["theme", "tab-width"]), Some("theme"));
        assert_eq!(closest("xyz", ["theme", "tab-width"]), None);
    }
}
