use super::*;
#[test]
fn dedup_keeps_first_occurrence_order() {
    assert_eq!(
        dedup_preserving_order(vec![MediaKind::Image, MediaKind::Text, MediaKind::Image]),
        vec![MediaKind::Image, MediaKind::Text]
    );
}
#[test]
fn dedup_collapses_adjacent_and_non_adjacent_repeats_alike() {
    assert_eq!(
        dedup_preserving_order(vec![MediaKind::Text, MediaKind::Text]),
        vec![MediaKind::Text]
    );
    assert_eq!(
        dedup_preserving_order(vec![
            MediaKind::Audio,
            MediaKind::Text,
            MediaKind::Audio,
            MediaKind::Text
        ]),
        vec![MediaKind::Audio, MediaKind::Text]
    );
}
#[test]
fn dedup_keeps_distinct_kinds_and_the_empty_list() {
    assert_eq!(
        dedup_preserving_order(vec![MediaKind::Text, MediaKind::Image, MediaKind::Audio]),
        vec![MediaKind::Text, MediaKind::Image, MediaKind::Audio]
    );
    assert!(dedup_preserving_order(Vec::new()).is_empty());
}
