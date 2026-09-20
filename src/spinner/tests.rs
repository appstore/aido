use super::{spinner_line, thousands};

#[test]
fn thousands_groups_in_threes() {
    assert_eq!(thousands(0), "0");
    assert_eq!(thousands(9), "9");
    assert_eq!(thousands(999), "999");
    assert_eq!(thousands(1204), "1,204");
    assert_eq!(thousands(1_000_000), "1,000,000");
}

#[test]
fn line_appends_count_only_once_content_arrives() {
    // A reasoning model sends no content at first: keep the bare
    // message rather than a misleading "0 chars".
    assert_eq!(spinner_line('⠋', "asking m...", 0), "⠋ asking m...");
    assert_eq!(
        spinner_line('⠙', "asking m...", 1204),
        "⠙ asking m... 1,204 chars"
    );
}
