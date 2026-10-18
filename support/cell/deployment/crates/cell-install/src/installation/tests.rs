use crate::artifact::inventory_matches;
use crate::{FileEntry, Manifest, Result};
use std::collections::BTreeMap;

#[test]
fn recorded_digest_and_signer_fields_do_not_gate_inventory_compatibility() -> Result<()> {
    let actual = BTreeMap::from([(
        "bin/usher".into(),
        FileEntry {
            sha256: String::new(),
            mode: 0o555,
            code_identifier: None,
        },
    )]);
    let mut retained = BTreeMap::from([(
        "bin/usher".into(),
        FileEntry {
            sha256: "retained predecessor digest".into(),
            mode: 0o555,
            code_identifier: Some("retained predecessor identifier".into()),
        },
    )]);
    assert!(inventory_matches(&actual, &retained));
    retained
        .get_mut("bin/usher")
        .ok_or(crate::Error::new("retained file entry is absent"))?
        .mode = 0o444;
    assert!(!inventory_matches(&actual, &retained));
    retained.clear();
    assert!(!inventory_matches(&actual, &retained));
    Ok(())
}

#[test]
fn predecessor_digests_are_read_as_metadata() -> Result<()> {
    let manifest: Manifest = serde_json::from_value(serde_json::json!({
        "format": "cell-install-v1", "product": "usher", "provider": "usher",
        "versions": {"usher": "0.3.0"},
        "files": {"bin/usher": {"sha256": "a".repeat(64), "mode": 493}},
        "release_id": "b".repeat(64),
    }))?;
    assert!(manifest.files["bin/usher"].code_identifier.is_none());
    assert_eq!(manifest.files["bin/usher"].sha256, "a".repeat(64));
    Ok(())
}
