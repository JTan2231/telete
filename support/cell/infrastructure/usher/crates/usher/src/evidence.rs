use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::inventory::{Product, valid_id, valid_slug};

const MAX_FILE_BYTES: u64 = 1_048_576;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Declared,
    Missing,
    Invalid,
    Unassessed,
}

impl Status {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Missing => "missing",
            Self::Invalid => "invalid",
            Self::Unassessed => "unassessed",
        }
    }
}

pub(crate) struct Problem {
    pub status: Status,
    pub(crate) message: String,
}

impl Problem {
    pub(crate) fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(Status::Invalid, message)
    }

    fn io(error: &std::io::Error) -> Self {
        let status = if error.kind() == std::io::ErrorKind::NotFound {
            Status::Missing
        } else {
            Status::Unassessed
        };
        Self::new(status, error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Issue {
    pub status: Status,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Finding {
    pub status: Status,
    pub identities: Vec<String>,
    pub evidence: Vec<String>,
    pub issues: Vec<Issue>,
}

impl Finding {
    pub(crate) fn new() -> Self {
        Self {
            status: Status::Declared,
            identities: vec![],
            evidence: vec![],
            issues: vec![],
        }
    }

    pub(crate) fn fail(&mut self, path: &str, problem: Problem) {
        self.status = self.status.max(problem.status);
        self.issues.push(Issue {
            status: problem.status,
            path: path.to_owned(),
            message: problem.message,
        });
    }

    pub(crate) fn descriptions(&self) -> impl Iterator<Item = String> + '_ {
        self.issues
            .iter()
            .map(|issue| format!("{}: {}", issue.path, issue.message))
    }
}

// Evidence stays under the selected root; selectors and symlinked material are
// not repository declarations. This is a local checkout reader, not a sandbox
// against a concurrent same-user filesystem attacker.
pub(crate) fn local_path(root: &Path, relative: &str) -> Result<PathBuf, Problem> {
    if relative.is_empty() || relative.contains('\\') || relative.chars().any(char::is_control) {
        return Err(Problem::invalid("expected a normal relative path"));
    }
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        let Component::Normal(name) = component else {
            return Err(Problem::invalid("path must remain beneath its owning root"));
        };
        path.push(name);
        if fs::symlink_metadata(&path)
            .map_err(|e| Problem::io(&e))?
            .file_type()
            .is_symlink()
        {
            return Err(Problem::invalid(
                "symbolic evidence paths are not supported",
            ));
        }
    }
    Ok(path)
}

pub(crate) fn directory(root: &Path, relative: &str) -> Result<PathBuf, Problem> {
    let path = local_path(root, relative)?;
    if !path.is_dir() {
        return Err(Problem::invalid("expected a directory"));
    }
    Ok(path)
}

pub(crate) fn read(root: &Path, relative: &str) -> Result<String, Problem> {
    let path = local_path(root, relative)?;
    if !fs::symlink_metadata(&path)
        .map_err(|e| Problem::io(&e))?
        .is_file()
    {
        return Err(Problem::invalid("expected a regular file"));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|e| Problem::io(&e))?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Problem::io(&e))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return Err(Problem::new(
            Status::Unassessed,
            "evidence exceeds the 1 MiB read limit",
        ));
    }
    String::from_utf8(bytes).map_err(|_| Problem::invalid("evidence must be UTF-8"))
}

pub(crate) fn semantics(root: &Path, product: &Product) -> Finding {
    let mut finding = Finding::new();
    let path = format!("{}/AGENTS.md", product.root);
    finding.evidence.push(path.clone());
    let text = match read(root, &path) {
        Ok(text) => text,
        Err(problem) => {
            finding.fail(&path, problem);
            return finding;
        }
    };
    let markers: Vec<&str> = text
        .lines()
        .filter(|line| line.trim_start().starts_with("Semantics-Project:"))
        .collect();
    match markers.as_slice() {
        [] => finding.fail(
            &path,
            Problem::new(Status::Missing, "no Semantics-Project declaration"),
        ),
        [line] => {
            if let Some(id) = line
                .strip_prefix("Semantics-Project: ")
                .filter(|id| valid_id(id))
            {
                finding.identities.push(id.to_owned());
            } else {
                finding.fail(
                    &path,
                    Problem::invalid(
                        "expected the exact line Semantics-Project: ID with a valid project ID",
                    ),
                );
            }
        }
        _ => finding.fail(
            &path,
            Problem::invalid("multiple Semantics-Project declarations"),
        ),
    }
    finding
}

// Chancery owns this partial wire view; Usher owns membership evidence policy.
fn introduction_error(_: serde_json::Error) -> Problem {
    Problem::invalid("malformed JSON or missing/invalid introduction fields")
}

fn inspect_provider(
    root: &Path,
    provider_dir: &str,
    id: &str,
    finding: &mut Finding,
) -> Result<(), Problem> {
    let path = format!("{provider_dir}/provider.json");
    let manifest = chancery::api::ProviderIntroduction::decode(&read(root, &path)?)
        .map_err(introduction_error)?;
    if !(1..=4).contains(&manifest.schema_version) {
        return Err(Problem::new(
            Status::Unassessed,
            "unsupported Chancery provider schema",
        ));
    }
    if manifest.provider.id != id
        || manifest.provider.name.trim().is_empty()
        || manifest.provider.release.trim().is_empty()
        || manifest.provider.name.chars().any(char::is_control)
        || manifest.provider.release.chars().any(char::is_control)
    {
        return Err(Problem::invalid(
            "provider identity must match its inventory declaration and include name and release",
        ));
    }
    if manifest.entries.is_empty() {
        return Err(Problem::new(Status::Missing, "provider indexes no entries"));
    }
    if manifest.entries.len() > 512 {
        return Err(Problem::new(
            Status::Unassessed,
            "provider exceeds the 512-entry read limit",
        ));
    }
    let bundle_root = directory(root, provider_dir)?;
    let mut paths = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for entry_path in manifest.entries {
        let evidence_path = format!("{provider_dir}/{entry_path}");
        if !paths.insert(entry_path.clone()) {
            finding.fail(
                &evidence_path,
                Problem::invalid("entry is indexed more than once"),
            );
            continue;
        }
        let result = (|| {
            let entry = chancery::api::EntryIntroduction::decode(&read(&bundle_root, &entry_path)?)
                .map_err(introduction_error)?;
            let prefix = format!("{id}.");
            if !entry
                .id
                .strip_prefix(&prefix)
                .is_some_and(|suffix| suffix.split('.').all(valid_slug))
                || entry.contract_version == 0
                || !ids.insert(entry.id)
            {
                return Err(Problem::invalid(
                    "invalid, foreign, or duplicate entry identity or contract version",
                ));
            }
            if read(&bundle_root, &entry.manual)?.trim().is_empty() {
                return Err(Problem::invalid("indexed manual is empty"));
            }
            Ok(())
        })();
        if let Err(problem) = result {
            finding.fail(&evidence_path, problem);
        }
    }
    Ok(())
}

pub(crate) fn chancery(root: &Path, product: &Product, descriptor: &str) -> Finding {
    let mut finding = Finding::new();
    if product.providers.trim().is_empty() {
        finding.fail(
            descriptor,
            Problem::new(Status::Missing, "no PROVIDERS declaration"),
        );
        return finding;
    }
    for row in product.providers.lines() {
        let columns: Vec<&str> = row.split('|').collect();
        let [unit, id, provider_dir, count] = columns.as_slice() else {
            finding.fail(
                descriptor,
                Problem::invalid("expected provider row unit|id|directory|entry-count"),
            );
            continue;
        };
        if !valid_slug(unit)
            || !valid_slug(id)
            || count.parse::<usize>().ok().is_none_or(|n| n == 0)
        {
            finding.fail(
                descriptor,
                Problem::invalid("invalid provider inventory row"),
            );
            continue;
        }
        finding.identities.push((*id).to_owned());
        let evidence_path = format!("{provider_dir}/provider.json");
        finding.evidence.push(evidence_path.clone());
        if !Path::new(provider_dir).starts_with(&product.root) {
            finding.fail(
                &evidence_path,
                Problem::invalid("provider bundle must belong to the product root"),
            );
            continue;
        }
        if let Err(problem) = inspect_provider(root, provider_dir, id, &mut finding) {
            finding.fail(&evidence_path, problem);
        }
    }
    finding
}
