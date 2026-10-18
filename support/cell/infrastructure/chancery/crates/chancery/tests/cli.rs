use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn owned_introduction_views_are_partial_and_keep_required_field_types() -> TestResult {
    use chancery::api::{EntryIntroduction, ProviderIntroduction, ProviderManifest};
    let provider = json!({
        "schema_version": 3,
        "provider": {"id": "alpha", "name": "Alpha", "release": "1", "extra": false},
        "entries": ["entries/read.json"], "promise_scope": false,
    });
    let view = ProviderIntroduction::decode(&provider.to_string())?;
    assert_eq!(view.provider.id, "alpha");
    assert!(ProviderManifest::decode(&provider.to_string()).is_err());
    assert!(EntryIntroduction::decode(r#"{"id":"alpha.read","contract_version":1,"manual":"manuals/read.md","dependencies":false}"#).is_ok());
    assert!(
        EntryIntroduction::decode(
            r#"{"id":"alpha.read","contract_version":"1","manual":"manuals/read.md"}"#
        )
        .is_err()
    );
    Ok(())
}
