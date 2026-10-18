use crate::model::{PackageId, ProductId, ProviderId};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Provider {
    pub(crate) id: ProviderId,
    pub(crate) path: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReleaseBinary {
    pub(crate) unit: String,
    pub(crate) source: PathBuf,
    pub(crate) name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Product {
    pub(crate) id: ProductId,
    pub(crate) name: String,
    pub(crate) root: PathBuf,
    pub(crate) aliases: Vec<String>,
    pub(crate) packages: Vec<PackageId>,
    pub(crate) offline: bool,
    pub(crate) providers: Vec<Provider>,
    pub(crate) binaries: Vec<ReleaseBinary>,
    pub(crate) fields: BTreeMap<String, String>,
}
#[derive(Debug, Clone)]
pub(crate) struct Inventory {
    pub(crate) products: Vec<Product>,
}

// Full CI descriptor projection. Values are literal data, never shell input.
fn assignments(text: &str) -> Result<BTreeMap<String, String>> {
    ensure!(
        text.len() <= 1024 * 1024 && !text.contains('\0'),
        "descriptor exceeds bounds"
    );
    let mut fields = BTreeMap::new();
    let mut remainder = text;
    while !remainder.is_empty() {
        let (line, tail) = remainder.split_once('\n').unwrap_or((remainder, ""));
        remainder = tail;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, raw) = line
            .split_once('=')
            .context("descriptor requires literal assignments")?;
        ensure!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'),
            "invalid descriptor field"
        );
        let value = if let Some(first) = raw.strip_prefix('\'') {
            let mut value = first.to_owned();
            while !value.ends_with('\'') {
                ensure!(!remainder.is_empty(), "unterminated descriptor literal");
                let (next, tail) = remainder.split_once('\n').unwrap_or((remainder, ""));
                remainder = tail;
                value.push('\n');
                value.push_str(next);
            }
            value.pop();
            ensure!(
                !value.contains('\''),
                "descriptor expression is unsupported"
            );
            value
        } else {
            ensure!(
                raw.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b)),
                "descriptor expression is unsupported"
            );
            raw.to_owned()
        };
        ensure!(
            fields.insert(name.to_owned(), value).is_none(),
            "duplicate descriptor field {name}"
        );
    }
    Ok(fields)
}

fn relative(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    ensure!(
        !path.as_os_str().is_empty()
            && !path.is_absolute()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "unsafe repository-relative path: {value}"
    );
    Ok(path)
}

pub(crate) fn checked_path(repo: &Path, relative: &Path) -> Result<PathBuf> {
    let mut path = repo.to_path_buf();
    for component in relative.components() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "unsafe relative component"
        );
        path.push(component);
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "symbolic repository declaration: {}",
            path.display()
        );
    }
    Ok(path)
}

impl Product {
    fn decode(text: &str, filename: &str) -> Result<Self> {
        let fields = assignments(text)?;
        let required = |key: &str| -> Result<&str> {
            fields
                .get(key)
                .map(String::as_str)
                .filter(|v| !v.is_empty())
                .with_context(|| format!("missing {key}"))
        };
        ensure!(
            required("PIPELINE_SCHEMA")? == "1",
            "unsupported pipeline schema"
        );
        let id = ProductId::new(required("PRODUCT_ID")?)?;
        ensure!(
            id.0 == filename,
            "product ID does not match descriptor filename"
        );
        let root = relative(required("PRODUCT_DIR")?)?;
        let name = required("PRODUCT_NAME")?.to_owned();
        let aliases = fields
            .get("PRODUCT_ALIASES")
            .map(|v| v.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
            .unwrap_or_default();
        for alias in &aliases {
            ProductId::new(alias)?;
        }
        let packages = required("CARGO_PACKAGES")?
            .split_whitespace()
            .map(PackageId::new)
            .collect::<Result<Vec<_>>>()?;
        let offline = match fields.get("CARGO_OFFLINE").map_or("0", String::as_str) {
            "0" => false,
            "1" => true,
            _ => bail!("invalid CARGO_OFFLINE"),
        };
        let mut providers = Vec::new();
        for row in fields
            .get("PROVIDERS")
            .into_iter()
            .flat_map(|v| v.lines())
            .filter(|r| !r.trim().is_empty())
        {
            let parts = row.split('|').collect::<Vec<_>>();
            ensure!(
                parts.len() == 4,
                "provider declaration requires four fields"
            );
            let path = relative(parts[2])?;
            ensure!(
                path.starts_with(&root),
                "provider lies outside its owning root"
            );
            providers.push(Provider {
                id: ProviderId::new(parts[1])?,
                path,
            });
        }
        ensure!(!providers.is_empty(), "product has no providers");
        let mut binaries = Vec::new();
        let mut names = BTreeSet::new();
        for row in fields
            .get("RELEASE_BINARY_CHECKS")
            .into_iter()
            .flat_map(|v| v.lines())
            .filter(|r| !r.trim().is_empty())
        {
            let parts = row.split('|').collect::<Vec<_>>();
            ensure!(
                parts.len() == 3 && !parts[0].is_empty() && names.insert(parts[2]),
                "invalid or duplicate release binary declaration"
            );
            PackageId::new(parts[2])?;
            binaries.push(ReleaseBinary {
                unit: parts[0].to_owned(),
                source: relative(parts[1])?,
                name: parts[2].to_owned(),
            });
        }
        Ok(Self {
            id,
            name,
            root,
            aliases,
            packages,
            offline,
            providers,
            binaries,
            fields,
        })
    }
}

impl Inventory {
    pub(crate) fn load(repo: &Path) -> Result<Self> {
        let directory = checked_path(repo, Path::new("pipeline/products"))?;
        let mut files = fs::read_dir(directory)?
            .map(|v| v.map(|v| v.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        files.retain(|v| v.extension().is_some_and(|e| e == "sh"));
        files.sort();
        if repo.join("infrastructure/telete/product.sh").is_file() {
            files.push(repo.join("infrastructure/telete/product.sh"));
        }
        ensure!(
            !files.is_empty() && files.len() <= 256,
            "missing or oversized product inventory"
        );
        let mut products = Vec::new();
        let mut identities = BTreeSet::new();
        let mut providers = BTreeSet::new();
        for file in files {
            let filename = if file.ends_with("infrastructure/telete/product.sh") {
                "telete"
            } else {
                file.file_stem()
                    .and_then(|v| v.to_str())
                    .context("invalid descriptor filename")?
            };
            let selected = checked_path(repo, file.strip_prefix(repo)?)?;
            ensure!(
                fs::metadata(&selected)?.is_file(),
                "descriptor must be regular"
            );
            let product = Product::decode(&fs::read_to_string(selected)?, filename)?;
            ensure!(
                checked_path(repo, &product.root)?.is_dir(),
                "product root is not a directory"
            );
            let claims: BTreeSet<_> = std::iter::once(&product.id.0)
                .chain(product.aliases.iter())
                .cloned()
                .collect();
            for identity in claims {
                ensure!(
                    identities.insert(identity.clone()),
                    "duplicate product identity or alias: {identity}"
                );
            }
            for provider in &product.providers {
                ensure!(
                    providers.insert(provider.id.clone()),
                    "multiple owners for provider {}",
                    provider.id
                );
            }
            ensure!(
                !products
                    .iter()
                    .any(|p: &Product| p.root.starts_with(&product.root)
                        || product.root.starts_with(&p.root)),
                "overlapping product roots"
            );
            products.push(product);
        }
        products.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Self { products })
    }
    pub(crate) fn select(&self, names: &[String]) -> Result<Vec<Product>> {
        let mut result = BTreeMap::new();
        for name in names {
            let product = self
                .products
                .iter()
                .find(|p| &p.id.0 == name || p.aliases.contains(name))
                .with_context(|| format!("unknown product: {name}"))?;
            result.insert(product.id.clone(), product.clone());
        }
        Ok(result.into_values().collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn descriptors_never_execute_shell_expressions() {
        assert!(assignments("PRODUCT_ID=$(touch /tmp/foreign)").is_err());
        assert!(assignments("A=1\nA=2").is_err());
        assert!(assignments("A=\"literal\"").is_err());
        assert_eq!(assignments("A='one\ntwo'\nB=0").unwrap()["A"], "one\ntwo");
    }
    #[test]
    fn path_escape_is_rejected() {
        assert!(relative("../other").is_err());
        assert!(relative("/absolute").is_err());
        assert!(relative("products/one").is_ok());
    }
    #[test]
    fn cell_descriptor_fixtures_keep_product_and_provider_identities_distinct() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("pipeline/products")).unwrap();
        fs::create_dir_all(root.join("products/decisions")).unwrap();
        fs::create_dir_all(root.join("infrastructure/telete")).unwrap();
        fs::write(
            root.join("pipeline/products/decisions.sh"),
            include_str!("../support/cell/pipeline/products/decisions.sh"),
        )
        .unwrap();
        fs::write(
            root.join("infrastructure/telete/product.sh"),
            include_str!("../product.sh"),
        )
        .unwrap();
        let inventory = Inventory::load(root).unwrap();
        let selected = inventory
            .select(&["krisis".into(), "decisions".into(), "telete".into()])
            .unwrap();
        assert_eq!(selected.len(), 2);
        let krisis = selected
            .iter()
            .find(|product| product.id.0 == "decisions")
            .unwrap();
        assert!(
            krisis
                .providers
                .iter()
                .any(|provider| provider.id.0 == "krisis")
        );
        assert_eq!(krisis.root, PathBuf::from("products/decisions"));
        assert!(selected.iter().any(|product| product.id.0 == "telete"));
    }
}
