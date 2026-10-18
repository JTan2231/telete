use std::collections::BTreeMap;
use std::path::Path;

use crate::evidence::{Problem, Status, directory, read};

pub(crate) struct Product {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) root: String,
    pub(crate) aliases: Vec<String>,
    pub(crate) providers: String,
    pub(crate) status_schema: Option<String>,
    pub(crate) status_command: Option<String>,
    pub(crate) status_units: Option<String>,
}

// Read the existing data-only descriptor format, never source it in a shell.
// Unquoted literals and whole single-quoted (possibly multiline) values are
// supported. Other shell syntax is deliberately unassessable, not executable.
fn assignments(text: &str) -> Result<BTreeMap<String, String>, Problem> {
    let mut fields = BTreeMap::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, raw) = line.split_once('=').ok_or_else(unsupported_syntax)?;
        if key.is_empty() || !key.bytes().all(|c| c.is_ascii_uppercase() || c == b'_') {
            return Err(unsupported_syntax());
        }
        let value = if let Some(first) = raw.strip_prefix('\'') {
            let mut value = first.to_owned();
            while !value.contains('\'') {
                value.push('\n');
                value.push_str(lines.next().ok_or_else(unsupported_syntax)?);
            }
            let Some(value) = value.strip_suffix('\'') else {
                return Err(unsupported_syntax());
            };
            if value.contains('\'') {
                return Err(unsupported_syntax());
            }
            value.to_owned()
        } else {
            if !raw
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_./:-".contains(&c))
            {
                return Err(unsupported_syntax());
            }
            raw.to_owned()
        };
        if fields.insert(key.to_owned(), value).is_some() {
            return Err(Problem::invalid("duplicate descriptor assignment"));
        }
    }
    Ok(fields)
}

fn unsupported_syntax() -> Problem {
    Problem::new(
        Status::Unassessed,
        "descriptor is not supported literal assignment data",
    )
}

pub(crate) fn valid_id(value: &str) -> bool {
    value.len() <= 64 && valid_slug(value)
}

pub(crate) fn valid_slug(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

pub(crate) fn load(root: &Path, descriptor: &str, file_id: &str) -> Result<Product, Problem> {
    let fields = assignments(&read(root, descriptor)?)?;
    let required = |key| {
        fields
            .get(key)
            .filter(|value| !value.is_empty())
            .cloned()
            .ok_or_else(|| Problem::invalid(format!("missing descriptor field {key}")))
    };
    if required("PIPELINE_SCHEMA")? != "1" {
        return Err(Problem::new(
            Status::Unassessed,
            "unsupported pipeline descriptor schema",
        ));
    }
    let id = required("PRODUCT_ID")?;
    if !valid_id(&id) || id != file_id {
        return Err(Problem::invalid(
            "PRODUCT_ID must match the descriptor filename and be a valid ID",
        ));
    }
    let name = required("PRODUCT_NAME")?;
    if name.trim().is_empty() || name.chars().any(char::is_control) {
        return Err(Problem::invalid(
            "PRODUCT_NAME must be a nonempty display name",
        ));
    }
    let product_root = required("PRODUCT_DIR")?;
    let product_path = directory(root, &product_root)?;
    let product_root = product_path
        .strip_prefix(root)
        .map_err(|_| Problem::invalid("product root escapes the checkout"))?
        .to_string_lossy()
        .into_owned();
    let aliases: Vec<String> = fields
        .get("PRODUCT_ALIASES")
        .into_iter()
        .flat_map(|value| value.split_whitespace().map(str::to_owned))
        .collect();
    if aliases.iter().any(|alias| !valid_id(alias)) {
        return Err(Problem::invalid("invalid product alias"));
    }
    Ok(Product {
        id,
        name,
        root: product_root,
        aliases,
        providers: fields.get("PROVIDERS").cloned().unwrap_or_default(),
        status_schema: fields.get("STATUS_SCHEMA").cloned(),
        status_command: fields.get("STATUS_COMMAND").cloned(),
        status_units: fields.get("STATUS_UNITS").cloned(),
    })
}
