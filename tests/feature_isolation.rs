#[test]
fn default_features_include_both_executables() {
    let manifest = include_str!("../Cargo.toml");
    assert!(manifest.contains("default = [\"http\", \"mqtt\", \"cli\"]"));
}
