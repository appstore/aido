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
