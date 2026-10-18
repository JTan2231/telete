//! Explicit prompt imports. Runtime readers never invoke this operation.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::api::{Reader, Writer};
use crate::prompts::{Error, Result, Selection};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Import {
    schema_version: u32,
    entries: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    content: String,
}

/// Counts for a completed explicit import.
#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub imported_ids: usize,
    pub appended_text_versions: usize,
    pub owner_selections: usize,
}

/// Import reviewed components and publish each owner's exact version selection.
///
/// # Errors
/// Invalid input fails before initialization. Database or storage failures can
/// leave committed components or owner selections; repeat the same reviewed
/// import to finish, preserving all prior versions.
pub fn import(database: &Path, input: &Path) -> Result<ImportReport> {
    let input: Import = serde_json::from_slice(&std::fs::read(input)?)?;
    if input.schema_version != 1 || input.entries.is_empty() {
        return Err(Error::Invalid("unsupported or empty import".into()));
    }
    let mut seen = BTreeSet::new();
    for entry in &input.entries {
        let owner = entry.id.split('.').next().unwrap_or("");
        if !matches!(
            owner,
            "annals" | "conatus" | "krisis" | "semantics" | "platter" | "weaver" | "emt" | "telete"
        ) || !seen.insert(entry.id.clone())
        {
            return Err(Error::Invalid(
                "unknown prompt owner or duplicate ID".into(),
            ));
        }
    }
    let mut writer = Writer::initialize(database)?;
    let reader = Reader::open(database)?;
    let mut owners: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    let mut appended = 0;
    for entry in input.entries {
        let record = match reader.get(&entry.id, None) {
            Ok(record) if record.content == entry.content => record,
            Ok(_) | Err(crate::api::Error::NotFound) => {
                appended += 1;
                writer.update(&entry.id, &entry.content)?
            }
            Err(error) => return Err(error.into()),
        };
        let owner = entry.id.split('.').next().unwrap_or("").to_owned();
        owners
            .entry(owner)
            .or_default()
            .insert(entry.id, record.version);
    }
    // Components exist before any selection is published. Repeating the same
    // import reuses identical latest records after an interrupted publication.
    for (owner, entries) in &owners {
        let id = format!("cell.prompts.{owner}");
        let content = serde_json::to_string(&Selection {
            schema_version: 1,
            entries: entries.clone(),
        })?;
        match reader.get(&id, None) {
            Ok(record) if record.content == content => {}
            Ok(_) | Err(crate::api::Error::NotFound) => {
                writer.update(&id, &content)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(ImportReport {
        imported_ids: seen.len(),
        appended_text_versions: appended,
        owner_selections: owners.len(),
    })
}
