#[test]
fn default_features_are_service_only() {
    let manifest = include_str!("../Cargo.toml");
    assert!(manifest.contains("default = [\"http\", \"mqtt\"]"));
}
