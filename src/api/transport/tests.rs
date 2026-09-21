use super::*;
#[test]
fn urls_keep_queries_and_proxy_paths() {
    assert_eq!(
        normalize_base_url("http://localhost:8080").unwrap(),
        "http://localhost:8080/v1"
    );
    assert_eq!(
        normalize_base_url("https://gw.example/proxy/?key=x").unwrap(),
        "https://gw.example/proxy?key=x"
    );
    assert!(normalize_base_url("file:///tmp/api").is_err());
    assert!(normalize_base_url("https://example.com/#x").is_err());
}

#[test]
fn cleartext_warning_names_the_host_off_loopback() {
    let warning = cleartext_key_warning(Some("http://gw.internal:8080/v1"), true).unwrap();
    assert!(warning.contains("cleartext"), "{warning}");
    assert!(warning.contains("gw.internal"), "{warning}");
}

#[test]
fn cleartext_warning_spares_loopback_https_and_keyless() {
    for url in [
        "http://localhost:8080/v1",
        "http://127.0.0.1:8080/v1",
        // The whole 127.0.0.0/8 block is loopback, not just .0.0.1.
        "http://127.250.1.9/v1",
        "http://[::1]:8080/v1",
        "https://gw.internal/v1",
    ] {
        assert!(
            cleartext_key_warning(Some(url), true).is_none(),
            "{url} must not warn"
        );
    }
    assert!(cleartext_key_warning(Some("http://gw.internal/v1"), false).is_none());
    assert!(cleartext_key_warning(None, true).is_none());
}
