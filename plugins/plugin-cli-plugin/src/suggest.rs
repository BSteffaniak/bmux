pub fn suggest_top_matches<'a>(
    target: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    limit: usize,
) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }

    let candidate_list = candidates.into_iter().collect::<Vec<_>>();
    if candidate_list.is_empty() {
        return Vec::new();
    }

    let lower_target = target.to_ascii_lowercase();
    let max_distance = lower_target.chars().count().max(3) / 2 + 1;

    let mut ranked = candidate_list
        .iter()
        .map(|candidate| {
            let lower_candidate = candidate.to_ascii_lowercase();
            let distance = levenshtein(&lower_target, &lower_candidate);
            let prefix_match = lower_candidate.starts_with(&lower_target)
                || lower_target.starts_with(&lower_candidate);
            (distance, !prefix_match, *candidate)
        })
        .filter(|(distance, prefix_penalty, _)| *distance <= max_distance || !*prefix_penalty)
        .collect::<Vec<_>>();

    ranked.sort_unstable();
    ranked
        .into_iter()
        .map(|(_, _, candidate)| candidate.to_string())
        .take(limit)
        .collect()
}

fn levenshtein(left: &str, right: &str) -> usize {
    let left_chars = left.chars().collect::<Vec<_>>();
    let right_chars = right.chars().collect::<Vec<_>>();
    if left_chars.is_empty() {
        return right_chars.len();
    }
    if right_chars.is_empty() {
        return left_chars.len();
    }

    let mut prev = (0..=right_chars.len()).collect::<Vec<_>>();
    let mut curr = vec![0; right_chars.len() + 1];
    for (i, l) in left_chars.iter().enumerate() {
        curr[0] = i + 1;
        for (j, r) in right_chars.iter().enumerate() {
            let cost = usize::from(l != r);
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[right_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::suggest_top_matches;

    #[test]
    fn suggest_top_matches_limits_and_filters_results() {
        let candidates = ["bmux.plugin_cli", "bmux.permissions", "bmux.windows"];
        let matches = suggest_top_matches("bmux.plugin", candidates.iter().copied(), 2);
        assert!(!matches.is_empty());
        assert_eq!(matches[0], "bmux.plugin_cli");
    }
}
