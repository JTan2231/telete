use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{inventory, report};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationalUnitDeclaration {
    pub id: String,
    pub intent: String,
    pub clockwork_key: Option<String>,
    pub inspection_capability: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationalProduct {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub descriptor: String,
    pub status_schema: Option<u32>,
    pub status_command: Option<String>,
    pub units: Vec<OperationalUnitDeclaration>,
    pub complete: bool,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationalInventory {
    pub schema_version: u32,
    pub scope: String,
    pub products: Vec<OperationalProduct>,
}

/// Read optional operational declarations through Usher's data-only parser.
///
/// This projection does not change Cell recognition and never invokes a probe.
///
/// # Errors
/// Returns a diagnostic when the Cell inventory cannot be established or the
/// requested product is unknown or ambiguous.
pub fn inspect_operations(
    root: &Path,
    selection: Option<&str>,
) -> Result<OperationalInventory, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot open checkout root: {error}"))?;
    let membership = report::inspect(&root, None)?;
    let mut products = membership
        .products
        .into_iter()
        .map(|member| {
            let mut output = OperationalProduct {
                id: member.id.clone(),
                name: member.name,
                aliases: member.aliases,
                descriptor: member.descriptor.clone(),
                status_schema: None,
                status_command: None,
                units: Vec::new(),
                complete: false,
                issues: Vec::new(),
            };
            match inventory::load(&root, &member.descriptor, &member.id) {
                Ok(product) => load_declaration(&product, &mut output),
                Err(problem) => output
                    .issues
                    .push(format!("declaration_unassessed: {}", problem.message)),
            }
            output.complete = output.issues.is_empty()
                && output.status_schema == Some(1)
                && output.status_command.is_some()
                && !output.units.is_empty();
            output
        })
        .collect::<Vec<_>>();

    let mut claims: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, product) in products.iter().enumerate() {
        for unit in &product.units {
            claims.entry(unit.id.clone()).or_default().push(index);
        }
    }
    for (unit, owners) in claims {
        if owners.len() < 2 {
            continue;
        }
        let names = owners
            .iter()
            .map(|index| products[*index].id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        for owner in owners {
            products[owner].complete = false;
            products[owner]
                .issues
                .push(format!("duplicate_unit: {unit} is declared by {names}"));
        }
    }

    if let Some(selection) = selection {
        products.retain(|product| {
            product.id == selection || product.aliases.iter().any(|alias| alias == selection)
        });
        if products.len() != 1 {
            return Err(format!(
                "product selection {selection:?} is unknown or ambiguous"
            ));
        }
    }

    Ok(OperationalInventory {
        schema_version: 1,
        scope: "repository_operational_declarations".to_owned(),
        products,
    })
}

fn load_declaration(product: &inventory::Product, output: &mut OperationalProduct) {
    match product.status_schema.as_deref() {
        Some(value) => match value.parse::<u32>() {
            Ok(1) => output.status_schema = Some(1),
            Ok(value) => {
                output.status_schema = Some(value);
                output
                    .issues
                    .push(format!("unsupported_status_schema: {value}"));
            }
            Err(_) => output
                .issues
                .push("invalid_status_schema: expected a positive integer".to_owned()),
        },
        None => output
            .issues
            .push("status_undeclared: STATUS_SCHEMA is absent".to_owned()),
    }

    match product.status_command.as_deref() {
        Some(command) if inventory::valid_slug(command) => {
            output.status_command = Some(command.to_owned());
        }
        Some(_) => output
            .issues
            .push("invalid_status_command: expected one executable selector basename".to_owned()),
        None => output
            .issues
            .push("status_undeclared: STATUS_COMMAND is absent".to_owned()),
    }

    let Some(units) = product.status_units.as_deref() else {
        output
            .issues
            .push("status_undeclared: STATUS_UNITS is absent".to_owned());
        return;
    };
    for (index, line) in units.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = line.split('|').collect::<Vec<_>>();
        if fields.len() != 4
            || !valid_unit_id(fields[0])
            || !matches!(
                fields[1],
                "active" | "on_demand" | "disabled" | "retired" | "unknown"
            )
            || (!fields[2].is_empty() && !valid_unit_id(fields[2]))
            || !valid_capability_id(fields[3])
        {
            output.issues.push(format!(
                "invalid_status_unit: STATUS_UNITS line {} is invalid",
                index + 1
            ));
            continue;
        }
        output.units.push(OperationalUnitDeclaration {
            id: fields[0].to_owned(),
            intent: fields[1].to_owned(),
            clockwork_key: (!fields[2].is_empty()).then(|| fields[2].to_owned()),
            inspection_capability: fields[3].to_owned(),
        });
    }
    if output.units.is_empty() {
        output
            .issues
            .push("status_undeclared: no operational units declared".to_owned());
    }
}

fn valid_unit_id(value: &str) -> bool {
    value.len() <= 129 && value.split('/').count() == 2 && value.split('/').all(inventory::valid_id)
}

fn valid_capability_id(value: &str) -> bool {
    let Some((provider, entry)) = value.split_once('.') else {
        return false;
    };
    inventory::valid_id(provider)
        && !entry.is_empty()
        && entry.len() <= 128
        && entry
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_unit_and_capability_identities() {
        assert!(valid_unit_id("annals/inbox"));
        assert!(!valid_unit_id("inbox"));
        assert!(valid_capability_id("annals.inbox.operate"));
        assert!(!valid_capability_id("annals"));
    }
}
