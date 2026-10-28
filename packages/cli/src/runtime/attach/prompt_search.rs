//! Domain-neutral filtering and ranking for searchable prompts.

use crate::runtime::prompt::{PromptOption, PromptSearchMatchMode};

pub(super) fn filtered_option_indices(
    options: &[PromptOption],
    query: &str,
    match_mode: PromptSearchMatchMode,
) -> Vec<usize> {
    let mut scored = options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            let search_text = option
                .search_text
                .as_deref()
                .unwrap_or(option.label.as_str());
            if let PromptSearchMatchMode::OrderedV1 {
                relevance: true, ..
            } = match_mode
            {
                let quality = match_quality(query, search_text).max(
                    option
                        .search_primary
                        .as_deref()
                        .map_or(0, |primary| match_quality(query, primary)),
                );
                search_score(query, search_text, match_mode)
                    .map(|score| (index, score, option.label.as_str(), quality))
            } else {
                search_score(query, search_text, match_mode)
                    .map(|score| (index, score, option.label.as_str(), 0))
            }
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        if let PromptSearchMatchMode::OrderedV1 { relevance, .. } = match_mode {
            let key = |index: usize| {
                options[index].search_order.map_or((0, index), |order| {
                    (
                        order.group,
                        if query.trim().is_empty() {
                            order.initial
                        } else {
                            order.filtered
                        },
                    )
                })
            };
            let left_key = key(left.0);
            let right_key = key(right.0);
            return left_key
                .0
                .cmp(&right_key.0)
                .then_with(|| {
                    if relevance && !query.trim().is_empty() {
                        right.3.cmp(&left.3).then_with(|| {
                            if left.3 == 0 {
                                right.1.cmp(&left.1)
                            } else {
                                std::cmp::Ordering::Equal
                            }
                        })
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| left_key.1.cmp(&right_key.1))
                .then_with(|| left.0.cmp(&right.0));
        }
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.2.cmp(right.2))
            .then_with(|| left.0.cmp(&right.0))
    });
    scored.into_iter().map(|(index, _, _, _)| index).collect()
}

// Classify generic searchable values, never product-specific label syntax.
// Literal matches always outrank a subsequence, regardless of fuzzy bonuses.
fn match_quality(query: &str, candidate: &str) -> u8 {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return 0;
    }
    let candidate = candidate.to_ascii_lowercase();
    if candidate == query {
        return 4;
    }
    candidate
        .match_indices(&query)
        .map(|(offset, _)| {
            let start_boundary = candidate[..offset]
                .chars()
                .next_back()
                .is_none_or(|ch| !ch.is_alphanumeric());
            let end_boundary = candidate[offset + query.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !ch.is_alphanumeric());
            if start_boundary && end_boundary {
                3
            } else if start_boundary {
                2
            } else {
                1
            }
        })
        .max()
        .unwrap_or(0)
}

fn search_score(query: &str, candidate: &str, match_mode: PromptSearchMatchMode) -> Option<i64> {
    let query = query.trim();
    if query.is_empty() {
        return Some(0);
    }
    match match_mode {
        PromptSearchMatchMode::OrderedV1 { matching, .. } => {
            use bmux_plugin_sdk::prompt::PromptOrderedMatchMode;
            search_score(
                query,
                candidate,
                match matching {
                    PromptOrderedMatchMode::Fuzzy => PromptSearchMatchMode::Fuzzy,
                    PromptOrderedMatchMode::Prefix => PromptSearchMatchMode::Prefix,
                    PromptOrderedMatchMode::Substring => PromptSearchMatchMode::Substring,
                },
            )
        }
        PromptSearchMatchMode::Fuzzy => fuzzy_score(query, candidate),
        PromptSearchMatchMode::Prefix => candidate
            .to_ascii_lowercase()
            .starts_with(&query.trim().to_ascii_lowercase())
            .then_some(0),
        PromptSearchMatchMode::Substring => candidate
            .to_ascii_lowercase()
            .contains(&query.trim().to_ascii_lowercase())
            .then_some(0),
    }
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let candidate_lower = candidate.to_ascii_lowercase();
    let mut last_match: Option<usize> = None;
    let mut score = 0_i64;
    let mut search_from = 0_usize;
    for needle in query.chars() {
        let haystack = &candidate_lower[search_from..];
        let found = haystack.find(needle)?;
        let absolute = search_from.saturating_add(found);
        if absolute == 0 {
            score += 100;
        } else if candidate_lower
            .as_bytes()
            .get(absolute.saturating_sub(1))
            .is_some_and(|byte| matches!(*byte, b' ' | b'-' | b'_' | b':' | b'/'))
        {
            score += 50;
        }
        if let Some(previous) = last_match {
            let gap = absolute.saturating_sub(previous).saturating_sub(1);
            score -= i64::try_from(gap).unwrap_or(i64::MAX / 4);
        }
        score += 10;
        last_match = Some(absolute);
        search_from = absolute.saturating_add(needle.len_utf8());
    }
    if candidate_lower.starts_with(&query) {
        score += 200;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEVANCE: PromptSearchMatchMode = PromptSearchMatchMode::OrderedV1 {
        matching: bmux_plugin_sdk::prompt::PromptOrderedMatchMode::Fuzzy,
        relevance: true,
    };

    #[test]
    fn literal_words_beat_boundary_bonuses_and_word_prefixes() {
        let options = ["p-a-u-s-e", "pauser", "unpaused", "Control: pause", "pause"]
            .map(|label| PromptOption::new(label, label));
        assert_eq!(
            filtered_option_indices(&options, " PAUSE ", RELEVANCE),
            [4, 3, 1, 2, 0]
        );
        // The legacy fuzzy mode is intentionally unchanged.
        let legacy = filtered_option_indices(&options, "pause", PromptSearchMatchMode::Fuzzy);
        assert!(
            legacy.iter().position(|index| *index == 0)
                < legacy.iter().position(|index| *index == 3)
        );
    }

    #[test]
    fn classification_checks_both_boundaries_and_all_occurrences() {
        for (text, quality) in [
            ("pause", 4),
            ("Pause output", 3),
            ("control:pause!", 3),
            ("pauser", 2),
            ("unpaused", 1),
            ("p-a-u-s-e", 0),
            ("pauser then pause", 3),
            ("épause", 1),
            ("é/pause", 3),
            ("pauseé", 2),
            ("pause_pause", 3),
        ] {
            assert_eq!(match_quality("pause", text), quality, "{text}");
        }
        assert_eq!(match_quality("pause output", "Control: pause output!"), 3);
        assert_eq!(match_quality("éé", "éé"), 4);
        assert!(fuzzy_score("éé", "é/é").is_some());
        assert!(fuzzy_score("aaa", "aa").is_none());
    }

    #[test]
    fn equal_quality_preserves_caller_order_and_primary_exactness() {
        let mut options = vec![
            PromptOption::new("second", "B: pause"),
            PromptOption::new("first", "A: pause"),
            PromptOption::new("exact", "Z: pause"),
        ];
        options[2].search_primary = Some("pause".into());
        assert_eq!(
            filtered_option_indices(&options, "pause", RELEVANCE),
            [2, 0, 1]
        );
        assert_eq!(filtered_option_indices(&options, "", RELEVANCE), [0, 1, 2]);
        assert_eq!(
            filtered_option_indices(&options, "  ", RELEVANCE),
            [0, 1, 2]
        );
    }

    #[test]
    fn display_metadata_does_not_add_search_keywords() {
        let options = [PromptOption::new("id", "unrelated")
            .detail("pause")
            .key_hint("pause")];
        assert_eq!(
            filtered_option_indices(&options, "pause", RELEVANCE),
            [] as [usize; 0]
        );
    }
    #[test]
    fn relevance_prefers_exact_primary_words_prefix_and_substring_over_fuzzy() {
        use bmux_plugin_sdk::prompt::{PromptOrderedMatchMode, PromptSearchOrder};
        let values = ["d-e-p-s", "mydeps", "update-deps", "deps-cleanup", "deps"];
        let options: Vec<_> = values
            .iter()
            .enumerate()
            .map(|(rank, value)| {
                let mut option = PromptOption::new(*value, format!("workspace/{value}"))
                    .search_text(format!("workspace/{value}"));
                option.search_primary = Some((*value).to_string());
                option.search_order = Some(PromptSearchOrder {
                    group: 0,
                    initial: rank,
                    filtered: rank,
                });
                option
            })
            .collect();
        let mode = PromptSearchMatchMode::OrderedV1 {
            matching: PromptOrderedMatchMode::Fuzzy,
            relevance: true,
        };
        // Both names contain a complete word; preserve caller order within that tier.
        assert_eq!(
            filtered_option_indices(&options, "deps", mode),
            vec![4, 2, 3, 1, 0]
        );
        assert_eq!(
            filtered_option_indices(&options, " DEPS ", mode),
            vec![4, 2, 3, 1, 0]
        );
        assert_eq!(
            filtered_option_indices(&options, "  ", mode),
            vec![0, 1, 2, 3, 4]
        );
        assert_eq!(
            filtered_option_indices(&options, "workspace/deps", mode)[..2],
            [4, 3]
        );
        let inherit = PromptSearchMatchMode::OrderedV1 {
            matching: PromptOrderedMatchMode::Fuzzy,
            relevance: false,
        };
        assert_eq!(
            filtered_option_indices(&options, "deps", inherit),
            vec![0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn search_match_modes_filter_against_custom_search_text() {
        let options = vec![
            PromptOption::new("editor", "project/editor").search_text("project editor"),
            PromptOption::new("shell", "other/shell").search_text("other shell"),
        ];

        assert_eq!(
            filtered_option_indices(&options, "pro ed", PromptSearchMatchMode::Fuzzy),
            vec![0]
        );
        assert_eq!(
            filtered_option_indices(&options, "project", PromptSearchMatchMode::Prefix),
            vec![0]
        );
        assert_eq!(
            filtered_option_indices(&options, "editor", PromptSearchMatchMode::Prefix),
            [] as [usize; 0]
        );
        assert_eq!(
            filtered_option_indices(&options, "editor", PromptSearchMatchMode::Substring),
            vec![0]
        );
        assert_eq!(
            filtered_option_indices(&options, "", PromptSearchMatchMode::Prefix),
            vec![1, 0]
        );
    }
}
