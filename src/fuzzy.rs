use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Indices of `items` matching `query`, best match first.
pub fn filter(query: &str, items: &[String]) -> Vec<usize> {
    if query.is_empty() {
        return (0..items.len()).collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(query, CaseMatching::Smart, Normalization::Smart, AtomKind::Fuzzy);
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, u32)> = items.iter().enumerate()
        .filter_map(|(i, s)| pattern.score(Utf32Str::new(s, &mut buf), &mut matcher).map(|score| (i, score)))
        .collect();
    // On a tie the shorter name wins, or `feat` couldn't be reached past `feat-2`.
    scored.sort_by_key(|&(i, score)| (std::cmp::Reverse(score), items[i].len(), i));
    scored.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_matches_and_drops_non_matches() {
        let items = vec!["feat-auth".to_string(), "fix-cache".to_string()];
        assert_eq!(filter("auth", &items), vec![0]);
    }

    #[test]
    fn empty_query_keeps_everything_in_order() {
        let items = vec!["b".to_string(), "a".to_string()];
        assert_eq!(filter("", &items), vec![0, 1]);
    }

    #[test]
    fn a_full_name_beats_a_longer_name_it_starts() {
        let items = vec!["feat-2".to_string(), "feat".to_string()];
        assert_eq!(filter("feat", &items), vec![1, 0]);
    }
}
