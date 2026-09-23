use super::expand_home;

#[test]
fn a_tilde_prefix_expands_to_the_home_directory() {
    let path = expand_home("~/models/asr");
    assert!(path.starts_with(dirs::home_dir().unwrap()));
    assert!(path.ends_with("models/asr"));
}

#[test]
fn other_forms_pass_through() {
    assert_eq!(
        expand_home("/abs/path"),
        std::path::PathBuf::from("/abs/path")
    );
    assert_eq!(
        expand_home("rel/path"),
        std::path::PathBuf::from("rel/path")
    );
    // A bare tilde is not the home spelling this config uses.
    assert_eq!(expand_home("~"), std::path::PathBuf::from("~"));
}
