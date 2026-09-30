mod guard_query_token {
    // The SPA URL-encodes the token into the `t` query parameter; a
    // fixed AIDO_UI_TOKEN may carry reserved bytes that must decode back
    // to what the server compares against.
    #[test]
    fn plain_and_percent_encoded_values_decode() {
        use super::super::guard::query_token;
        assert_eq!(query_token("a=1&t=secret&b=2").as_deref(), Some("secret"));
        assert_eq!(query_token("t=abc%2Fdef").as_deref(), Some("abc/def"));
        assert_eq!(query_token("t=a+b").as_deref(), Some("a b"));
        assert_eq!(query_token("t=%e4%b8%ad").as_deref(), Some("中"));
        // A truncated escape is someone else's query string, not ours.
        assert_eq!(query_token("t=100%"), None);
        assert_eq!(query_token("x=1"), None);
    }
}

mod task_file_stem {
    use super::super::api::task_file_stem;

    #[test]
    fn plain_stems_pass_with_surrounding_space_trimmed() {
        assert_eq!(task_file_stem("daily-report").unwrap(), "daily-report");
        assert_eq!(task_file_stem("  tts_fast  ").unwrap(), "tts_fast");
        assert_eq!(task_file_stem("v2.report").unwrap(), "v2.report");
        // Shadowing a built-in is load_all's own override rule, not a
        // name problem.
        assert_eq!(task_file_stem("ocr").unwrap(), "ocr");
    }

    #[test]
    fn path_shapes_and_empty_names_are_refused() {
        for bad in [
            "",
            "   ",
            "-flag",
            ".hidden",
            "a/b",
            "a\\b",
            "a b",
            "..",
            "中文名",
            &"x".repeat(65),
        ] {
            assert!(task_file_stem(bad).is_err(), "'{bad}' should be refused");
        }
        // 64 bytes is the ceiling, and it passes.
        assert!(task_file_stem(&"x".repeat(64)).is_ok());
    }
}
