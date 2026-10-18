use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
use std::path::{Component, Path};

use sha2::{Digest as _, Sha256};
use unicode_normalization::UnicodeNormalization as _;

use crate::error::AppError;
use crate::model::{
    ClaimStatus, DependencyState, DependencyStatus, EntryDocument, EntryKind, EntryPromise, Issue,
    LEGACY_PROVIDER_SCHEMA_VERSION, LoadedEntry, PREVIOUS_PROVIDER_SCHEMA_VERSION,
    PROVIDER_SCHEMA_VERSION, PromiseClaim, ProviderBundle, ProviderManifest, ProviderPromiseScope,
    Registry, RelianceClaim, V2_PROVIDER_SCHEMA_VERSION,
};

const MAX_MANIFEST_BYTES: u64 = 1_048_576;
const MAX_ENTRY_BYTES: u64 = 1_048_576;
const MAX_MANUAL_BYTES: u64 = 4_194_304;
const MAX_PROVIDERS: usize = 1_024;
const MAX_ENTRIES_PER_PROVIDER: usize = 4_096;
const MAX_FIELD_BYTES: usize = 65_536;

pub(crate) fn load_registry(registry_path: &Path) -> Result<Registry, AppError> {
    let root = canonical_registry(registry_path)?;
    let children = registry_children(&root)?;
    let mut providers = Vec::new();
    let mut issues = Vec::new();
    let mut scanned_providers = 0;
    for child in children {
        if child
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        scanned_providers += 1;
        if let Some(provider) = load_registry_child(&child, &mut issues) {
            providers.push(provider);
        }
    }

    exclude_duplicate_entries(&mut providers, &mut issues);
    calculate_dependencies(&mut providers, &mut issues);

    Ok(Registry {
        root,
        providers,
        issues,
        scanned_providers,
    })
}

fn canonical_registry(registry_path: &Path) -> Result<std::path::PathBuf, AppError> {
    if !registry_path.is_absolute() {
        return Err(AppError::invalid(
            "invalid_registry_path",
            format!(
                "registry path must be absolute: {}",
                registry_path.display()
            ),
        ));
    }
    let root = fs::canonicalize(registry_path).map_err(|error| {
        AppError::invalid(
            "registry_unavailable",
            format!(
                "unable to resolve registry {}: {error}",
                registry_path.display()
            ),
        )
    })?;
    if !root.is_dir() {
        return Err(AppError::invalid(
            "registry_unavailable",
            format!("registry is not a directory: {}", root.display()),
        ));
    }
    Ok(root)
}

fn registry_children(root: &Path) -> Result<Vec<std::path::PathBuf>, AppError> {
    let directory = fs::read_dir(root).map_err(|error| {
        AppError::invalid(
            "registry_unavailable",
            format!("unable to read registry {}: {error}", root.display()),
        )
    })?;
    let mut children = Vec::new();
    for child in directory {
        let child = child.map_err(|error| {
            AppError::invalid(
                "registry_unavailable",
                format!("unable to enumerate registry {}: {error}", root.display()),
            )
        })?;
        children.push(child.path());
        if children.len() > MAX_PROVIDERS {
            return Err(AppError::invalid(
                "registry_too_large",
                format!("registry contains more than {MAX_PROVIDERS} providers"),
            ));
        }
    }
    children.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
    Ok(children)
}

fn load_registry_child(child: &Path, issues: &mut Vec<Issue>) -> Option<ProviderBundle> {
    let Some(name) = child.file_name().and_then(|value| value.to_str()) else {
        issues.push(
            Issue::new(
                "invalid_registry_entry",
                "registry entry name is not valid UTF-8",
            )
            .path(child.display().to_string()),
        );
        return None;
    };
    if !valid_slug(name) {
        issues.push(
            Issue::new(
                "invalid_provider_id",
                format!("registry entry is not a valid provider ID: {name}"),
            )
            .provider(name)
            .path(child.display().to_string()),
        );
        return None;
    }
    let target = match fs::canonicalize(child) {
        Ok(target) => target,
        Err(error) => {
            issues.push(
                Issue::new(
                    "provider_unavailable",
                    format!("unable to resolve provider {name}: {error}"),
                )
                .provider(name)
                .path(child.display().to_string()),
            );
            return None;
        }
    };
    match load_bundle(&target) {
        Ok(provider) if provider.identity.id == name => Some(provider),
        Ok(provider) => {
            issues.push(
                Issue::new(
                    "provider_id_mismatch",
                    format!(
                        "registry entry {name} contains provider {}",
                        provider.identity.id
                    ),
                )
                .provider(name)
                .path(child.display().to_string()),
            );
            None
        }
        Err(mut provider_issues) => {
            attach_provider(&mut provider_issues, name);
            issues.extend(provider_issues);
            None
        }
    }
}

pub(crate) fn load_bundle(bundle_path: &Path) -> Result<ProviderBundle, Vec<Issue>> {
    let (root, manifest, manifest_sha256) = load_manifest(bundle_path)?;
    let mut issues = validate_manifest(&manifest);
    if manifest.entries.len() > MAX_ENTRIES_PER_PROVIDER {
        issues.push(Issue::new(
            "provider_too_large",
            format!(
                "provider {} indexes more than {MAX_ENTRIES_PER_PROVIDER} entries",
                manifest.provider.id
            ),
        ));
    }
    if !issues.is_empty() {
        attach_provider(&mut issues, &manifest.provider.id);
        return Err(issues);
    }
    let mut entries = load_entries(&root, &manifest, &mut issues);
    let overview_text = manifest
        .overview
        .as_deref()
        .map(|path| load_overview(&root, path, &mut issues));
    if !issues.is_empty() {
        attach_provider(&mut issues, &manifest.provider.id);
        return Err(issues);
    }
    entries.sort_by(|left, right| left.document.id.cmp(&right.document.id));
    Ok(ProviderBundle {
        schema_version: manifest.schema_version,
        identity: manifest.provider,
        promise_scope: manifest.promise_scope,
        overview_text,
        root,
        manifest_sha256,
        entries,
    })
}

pub(crate) fn validate_internal_dependencies(provider: &ProviderBundle) -> Vec<Issue> {
    let catalog: BTreeMap<_, _> = provider
        .entries
        .iter()
        .map(|entry| (entry.document.id.clone(), entry.document.contract_version))
        .collect();
    let adjacency: BTreeMap<_, _> = provider
        .entries
        .iter()
        .map(|entry| {
            (
                entry.document.id.clone(),
                entry
                    .document
                    .dependencies
                    .iter()
                    .filter(|dependency| catalog.contains_key(&dependency.id))
                    .map(|dependency| dependency.id.clone())
                    .collect(),
            )
        })
        .collect();
    let cycle_nodes = dependency_cycle_nodes(&adjacency);
    let mut issues = Vec::new();

    for entry in &provider.entries {
        for dependency in &entry.document.dependencies {
            let Some(installed_contract) = catalog.get(&dependency.id).copied() else {
                continue;
            };
            let state = if cycle_nodes.contains(&entry.document.id)
                && cycle_nodes.contains(&dependency.id)
                && dependency_edge_is_cyclic(&entry.document.id, &dependency.id, &adjacency)
            {
                DependencyState::Cycle
            } else if installed_contract < dependency.min_contract
                || installed_contract >= dependency.max_contract_exclusive
            {
                DependencyState::Incompatible
            } else {
                DependencyState::Compatible
            };
            if state != DependencyState::Compatible {
                let (code, message) = dependency_issue(
                    &entry.document.id,
                    dependency,
                    Some(installed_contract),
                    state,
                );
                issues.push(
                    Issue::new(code, message)
                        .provider(&provider.identity.id)
                        .entry(&entry.document.id),
                );
            }
        }
    }

    issues
}

fn load_manifest(
    bundle_path: &Path,
) -> Result<(std::path::PathBuf, ProviderManifest, String), Vec<Issue>> {
    let root = fs::canonicalize(bundle_path).map_err(|error| {
        vec![
            Issue::new(
                "bundle_unavailable",
                format!(
                    "unable to resolve bundle {}: {error}",
                    bundle_path.display()
                ),
            )
            .path(bundle_path.display().to_string()),
        ]
    })?;
    if !root.is_dir() {
        return Err(vec![
            Issue::new(
                "bundle_unavailable",
                format!("bundle is not a directory: {}", root.display()),
            )
            .path(root.display().to_string()),
        ]);
    }
    let manifest_text = read_bundle_file(
        &root,
        "provider.json",
        MAX_MANIFEST_BYTES,
        "manifest_unavailable",
    )?;
    let manifest = ProviderManifest::decode(&manifest_text).map_err(|error| {
        vec![
            Issue::new(
                "invalid_manifest",
                format!("invalid provider.json: {error}"),
            )
            .path(root.join("provider.json").display().to_string()),
        ]
    })?;
    let manifest_sha256 = sha256(&manifest_text);
    Ok((root, manifest, manifest_sha256))
}

fn load_entries(
    root: &Path,
    manifest: &ProviderManifest,
    issues: &mut Vec<Issue>,
) -> Vec<LoadedEntry> {
    let mut entries = Vec::with_capacity(manifest.entries.len());
    let mut indexed_paths = BTreeSet::new();
    let mut entry_ids = BTreeSet::new();
    for indexed_path in &manifest.entries {
        if !indexed_paths.insert(normalized(indexed_path)) {
            issues.push(
                Issue::new(
                    "duplicate_entry_path",
                    format!("provider indexes entry path more than once: {indexed_path}"),
                )
                .path(indexed_path),
            );
            continue;
        }
        match load_entry(
            root,
            manifest.schema_version,
            &manifest.provider.id,
            indexed_path,
            &mut entry_ids,
        ) {
            Ok(entry) => entries.push(entry),
            Err(mut entry_issues) => issues.append(&mut entry_issues),
        }
    }
    entries
}

fn load_entry(
    root: &Path,
    schema_version: u32,
    provider_id: &str,
    indexed_path: &str,
    entry_ids: &mut BTreeSet<String>,
) -> Result<LoadedEntry, Vec<Issue>> {
    if !path_has_parent(indexed_path, "entries", "json") {
        return Err(vec![
            Issue::new(
                "invalid_entry_path",
                format!("entry path must be entries/NAME.json: {indexed_path}"),
            )
            .path(indexed_path),
        ]);
    }
    let entry_text = read_bundle_file(root, indexed_path, MAX_ENTRY_BYTES, "entry_unavailable")?;
    let document = parse_entry(&entry_text, schema_version).map_err(|error| {
        vec![
            Issue::new(
                "invalid_entry",
                format!("invalid entry {indexed_path}: {error}"),
            )
            .path(indexed_path),
        ]
    })?;
    let mut issues = validate_entry(schema_version, provider_id, &document);
    if !entry_ids.insert(document.id.clone()) {
        issues.push(
            Issue::new(
                "duplicate_entry_id",
                format!("provider defines entry more than once: {}", document.id),
            )
            .entry(&document.id),
        );
    }
    let manual_text = load_manual(root, &document, &mut issues);
    if issues.is_empty() {
        Ok(LoadedEntry {
            document,
            source_path: indexed_path.to_owned(),
            source_sha256: sha256(&entry_text),
            manual_sha256: sha256(&manual_text),
            manual_text,
            dependency_statuses: Vec::new(),
            compatible: true,
        })
    } else {
        Err(issues)
    }
}

fn parse_entry(entry_text: &str, schema_version: u32) -> Result<EntryDocument, serde_json::Error> {
    EntryDocument::decode(entry_text, schema_version)
}

fn load_manual(root: &Path, document: &EntryDocument, issues: &mut Vec<Issue>) -> String {
    if !path_has_parent(&document.manual, "manuals", "md") {
        issues.push(
            Issue::new(
                "invalid_manual_path",
                format!("manual path must be manuals/NAME.md: {}", document.manual),
            )
            .entry(&document.id)
            .path(&document.manual),
        );
        return String::new();
    }
    let text = match read_bundle_file(
        root,
        &document.manual,
        MAX_MANUAL_BYTES,
        "manual_unavailable",
    ) {
        Ok(text) => text,
        Err(mut file_issues) => {
            issues.append(&mut file_issues);
            return String::new();
        }
    };
    if text.trim().is_empty() {
        issues.push(
            Issue::new(
                "invalid_manual",
                format!("manual is empty: {}", document.manual),
            )
            .entry(&document.id)
            .path(&document.manual),
        );
    }
    if let Some(character) = text
        .chars()
        .find(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        issues.push(
            Issue::new(
                "invalid_manual",
                format!(
                    "manual contains unsupported control character U+{:04X}",
                    u32::from(character)
                ),
            )
            .entry(&document.id)
            .path(&document.manual),
        );
    }
    text
}

fn load_overview(root: &Path, path: &str, issues: &mut Vec<Issue>) -> String {
    if !safe_relative_path(Path::new(path))
        || Path::new(path).extension().and_then(|value| value.to_str()) != Some("md")
    {
        issues.push(
            Issue::new(
                "invalid_overview_path",
                format!("overview path must be a safe relative Markdown path: {path}"),
            )
            .path(path),
        );
        return String::new();
    }
    let text = match read_bundle_file(root, path, MAX_MANUAL_BYTES, "overview_unavailable") {
        Ok(text) => text,
        Err(mut file_issues) => {
            issues.append(&mut file_issues);
            return String::new();
        }
    };
    if text.trim().is_empty() {
        issues.push(Issue::new("invalid_overview", "overview must not be empty").path(path));
    }
    if let Some(character) = text
        .chars()
        .find(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        issues.push(
            Issue::new(
                "invalid_overview",
                format!(
                    "overview contains unsupported control character U+{:04X}",
                    u32::from(character)
                ),
            )
            .path(path),
        );
    }
    text
}

fn validate_manifest(manifest: &ProviderManifest) -> Vec<Issue> {
    let mut issues = Vec::new();
    if !matches!(
        manifest.schema_version,
        LEGACY_PROVIDER_SCHEMA_VERSION
            | V2_PROVIDER_SCHEMA_VERSION
            | PREVIOUS_PROVIDER_SCHEMA_VERSION
            | PROVIDER_SCHEMA_VERSION
    ) {
        issues.push(Issue::new(
            "unsupported_schema",
            format!(
                "provider schema {} is unsupported; supported schemas are {}, {}, {}, and {}",
                manifest.schema_version,
                LEGACY_PROVIDER_SCHEMA_VERSION,
                V2_PROVIDER_SCHEMA_VERSION,
                PREVIOUS_PROVIDER_SCHEMA_VERSION,
                PROVIDER_SCHEMA_VERSION
            ),
        ));
    }
    if !valid_slug(&manifest.provider.id) {
        issues.push(Issue::new(
            "invalid_provider_id",
            format!("invalid provider ID: {}", manifest.provider.id),
        ));
    }
    validate_text("provider name", &manifest.provider.name, &mut issues, None);
    validate_text(
        "provider release",
        &manifest.provider.release,
        &mut issues,
        None,
    );
    if manifest.entries.is_empty() {
        issues.push(Issue::new(
            "empty_provider",
            "provider must index at least one entry",
        ));
    }
    match (
        manifest.schema_version,
        manifest.promise_scope_present,
        &manifest.promise_scope,
    ) {
        (PREVIOUS_PROVIDER_SCHEMA_VERSION | PROVIDER_SCHEMA_VERSION, true, Some(scope)) => {
            validate_provider_scope(&manifest.provider.id, scope, &mut issues);
        }
        (PREVIOUS_PROVIDER_SCHEMA_VERSION | PROVIDER_SCHEMA_VERSION, _, None) => issues.push(
            Issue::new(
                "missing_promise_scope",
                "provider schemas 3 and 4 require promise_scope to be an object",
            )
            .provider(&manifest.provider.id),
        ),
        (LEGACY_PROVIDER_SCHEMA_VERSION | V2_PROVIDER_SCHEMA_VERSION, true, _) => issues.push(
            Issue::new(
                "unexpected_promise_scope",
                "promise_scope requires provider schema 3 or 4",
            )
            .provider(&manifest.provider.id),
        ),
        _ => {}
    }
    if manifest.overview_present {
        if manifest.schema_version != PROVIDER_SCHEMA_VERSION {
            issues.push(Issue::new(
                "unexpected_overview",
                "overview requires provider schema 4",
            ));
        } else if manifest.overview.is_none() {
            issues.push(Issue::new(
                "invalid_overview",
                "overview must be a Markdown path or omitted",
            ));
        }
    }
    issues
}

fn validate_entry(schema_version: u32, provider_id: &str, entry: &EntryDocument) -> Vec<Issue> {
    let mut issues = Vec::new();
    validate_entry_identity(provider_id, entry, &mut issues);
    validate_entry_text(entry, &mut issues);
    validate_interfaces(entry, &mut issues);
    validate_dependencies(entry, &mut issues);
    validate_entry_kind(entry, &mut issues);
    match (schema_version, entry.promise_present, &entry.promise) {
        (PREVIOUS_PROVIDER_SCHEMA_VERSION | PROVIDER_SCHEMA_VERSION, true, Some(promise)) => {
            validate_promise(entry, promise, &mut issues);
        }
        (PREVIOUS_PROVIDER_SCHEMA_VERSION | PROVIDER_SCHEMA_VERSION, true, None) => issues.push(
            Issue::new(
                "invalid_promise_declaration",
                "promise must be an object or omitted",
            )
            .entry(&entry.id),
        ),
        (LEGACY_PROVIDER_SCHEMA_VERSION | V2_PROVIDER_SCHEMA_VERSION, true, _) => issues.push(
            Issue::new(
                "unexpected_promise_declaration",
                "entry promise requires provider schema 3 or 4",
            )
            .entry(&entry.id),
        ),
        _ => {}
    }
    issues
}

fn validate_provider_scope(
    provider_id: &str,
    scope: &ProviderPromiseScope,
    issues: &mut Vec<Issue>,
) {
    for (label, values) in [
        ("authoritative_for", scope.authoritative_for.as_slice()),
        (
            "not_authoritative_for",
            scope.not_authoritative_for.as_slice(),
        ),
        ("inventory.covers", scope.inventory.covers.as_slice()),
        ("inventory.excludes", scope.inventory.excludes.as_slice()),
        (
            "shared_access_and_trust",
            scope.shared_access_and_trust.as_slice(),
        ),
        (
            "shared_privacy_and_retention",
            scope.shared_privacy_and_retention.as_slice(),
        ),
        (
            "compatibility_and_retirement",
            scope.compatibility_and_retirement.as_slice(),
        ),
        ("operational_limits", scope.operational_limits.as_slice()),
    ] {
        validate_provider_list(label, values, provider_id, issues);
    }
}

fn validate_provider_list(
    label: &str,
    values: &[String],
    provider_id: &str,
    issues: &mut Vec<Issue>,
) {
    if values.is_empty() {
        issues.push(
            Issue::new(
                "empty_field",
                format!("promise_scope.{label} must not be empty"),
            )
            .provider(provider_id),
        );
        return;
    }
    let mut unique = BTreeSet::new();
    for value in values {
        let before = issues.len();
        validate_text(label, value, issues, None);
        for issue in &mut issues[before..] {
            issue.provider = Some(provider_id.to_owned());
        }
        if !unique.insert(normalized(value)) {
            issues.push(
                Issue::new(
                    "duplicate_value",
                    format!("provider {provider_id} repeats a value in promise_scope.{label}"),
                )
                .provider(provider_id),
            );
        }
    }
}

fn validate_promise(entry: &EntryDocument, promise: &EntryPromise, issues: &mut Vec<Issue>) {
    for (label, claims) in [
        ("consumers", promise.consumers.as_slice()),
        ("preconditions", promise.preconditions.as_slice()),
        ("inputs", promise.inputs.as_slice()),
        ("outputs", promise.outputs.as_slice()),
        ("data_semantics", promise.data_semantics.as_slice()),
        ("identity_and_units", promise.identity_and_units.as_slice()),
        (
            "completeness_and_freshness",
            promise.completeness_and_freshness.as_slice(),
        ),
        ("access", promise.access.as_slice()),
        (
            "lifecycle_and_consistency",
            promise.lifecycle_and_consistency.as_slice(),
        ),
        ("operational_limits", promise.operational_limits.as_slice()),
        (
            "compatibility_and_evolution",
            promise.compatibility_and_evolution.as_slice(),
        ),
    ] {
        validate_promise_claims(label, claims, &entry.id, issues);
    }
    validate_reliance_claims(entry, &promise.reliances, issues);
}

fn validate_promise_claims(
    label: &str,
    claims: &[PromiseClaim],
    entry_id: &str,
    issues: &mut Vec<Issue>,
) {
    if claims.is_empty() {
        issues.push(
            Issue::new(
                "empty_field",
                format!("promise.{label} must contain at least one explicit claim"),
            )
            .entry(entry_id),
        );
        return;
    }
    let mut unique = BTreeSet::new();
    for claim in claims {
        validate_text(
            &format!("promise.{label} statement"),
            &claim.statement,
            issues,
            Some(entry_id),
        );
        if !unique.insert(normalized(&claim.statement)) {
            issues.push(
                Issue::new(
                    "duplicate_value",
                    format!("entry {entry_id} repeats a statement in promise.{label}"),
                )
                .entry(entry_id),
            );
        }
    }
}

fn validate_reliance_claims(
    entry: &EntryDocument,
    claims: &[RelianceClaim],
    issues: &mut Vec<Issue>,
) {
    if claims.is_empty() {
        issues.push(
            Issue::new(
                "empty_field",
                "promise.reliances must contain at least one explicit claim",
            )
            .entry(&entry.id),
        );
        return;
    }
    let dependency_ids: BTreeSet<_> = entry
        .dependencies
        .iter()
        .map(|dependency| dependency.id.as_str())
        .collect();
    let mut unique = BTreeSet::new();
    for claim in claims {
        validate_text(
            "promise.reliances statement",
            &claim.statement,
            issues,
            Some(&entry.id),
        );
        if !unique.insert(normalized(&claim.statement)) {
            issues.push(
                Issue::new(
                    "duplicate_value",
                    format!(
                        "entry {} repeats a statement in promise.reliances",
                        entry.id
                    ),
                )
                .entry(&entry.id),
            );
        }
        validate_reliance_metadata(entry, claim, &dependency_ids, issues);
    }
}

fn validate_reliance_metadata(
    entry: &EntryDocument,
    claim: &RelianceClaim,
    dependency_ids: &BTreeSet<&str>,
    issues: &mut Vec<Issue>,
) {
    if claim.status != ClaimStatus::Declared {
        if claim.target.is_some() || claim.kind.is_some() || claim.contract.is_some() {
            issues.push(
                Issue::new(
                    "unexpected_reliance_metadata",
                    format!(
                        "entry {} may attach target, kind, and contract only to a declared reliance",
                        entry.id
                    ),
                )
                .entry(&entry.id),
            );
        }
        return;
    }

    match &claim.target {
        Some(target) if valid_slug(target) => {}
        Some(target) => issues.push(
            Issue::new(
                "invalid_reliance_target",
                format!("entry {} has invalid reliance target {target}", entry.id),
            )
            .entry(&entry.id),
        ),
        None => issues.push(
            Issue::new(
                "missing_reliance_target",
                format!(
                    "entry {} has a declared reliance without a target",
                    entry.id
                ),
            )
            .entry(&entry.id),
        ),
    }
    if claim.kind.is_none() {
        issues.push(
            Issue::new(
                "missing_reliance_kind",
                format!("entry {} has a declared reliance without a kind", entry.id),
            )
            .entry(&entry.id),
        );
    }
    let Some(contract) = &claim.contract else {
        return;
    };
    if !valid_entry_id(contract) {
        issues.push(
            Issue::new(
                "invalid_reliance_contract",
                format!(
                    "entry {} has invalid reliance contract {contract}",
                    entry.id
                ),
            )
            .entry(&entry.id),
        );
    } else if !dependency_ids.contains(contract.as_str()) {
        issues.push(
            Issue::new(
                "unbounded_reliance_contract",
                format!(
                    "entry {} reliance contract {contract} must also be a versioned dependency",
                    entry.id
                ),
            )
            .entry(&entry.id),
        );
    }
}

fn validate_entry_identity(provider_id: &str, entry: &EntryDocument, issues: &mut Vec<Issue>) {
    if !valid_entry_id(&entry.id) {
        issues.push(
            Issue::new(
                "invalid_entry_id",
                format!("invalid entry ID: {}", entry.id),
            )
            .entry(&entry.id),
        );
    } else if !entry.id.starts_with(&format!("{provider_id}.")) {
        issues.push(
            Issue::new(
                "entry_provider_mismatch",
                format!(
                    "entry {} is not namespaced to provider {provider_id}",
                    entry.id
                ),
            )
            .entry(&entry.id),
        );
    }
    if entry.contract_version == 0 {
        issues.push(
            Issue::new(
                "invalid_contract_version",
                "contract version must be positive",
            )
            .entry(&entry.id),
        );
    }
}

fn validate_entry_text(entry: &EntryDocument, issues: &mut Vec<Issue>) {
    for (label, value) in [
        ("title", entry.title.as_str()),
        ("summary", entry.summary.as_str()),
        ("outcome", entry.outcome.as_str()),
    ] {
        validate_text(label, value, issues, Some(&entry.id));
    }
    for (label, values) in [
        ("use_when", entry.use_when.as_slice()),
        ("do_not_use_when", entry.do_not_use_when.as_slice()),
        ("effects", entry.effects.as_slice()),
        ("authority", entry.authority.as_slice()),
        ("success", entry.success.as_slice()),
        (
            "failure_and_recovery",
            entry.failure_and_recovery.as_slice(),
        ),
        ("privacy", entry.privacy.as_slice()),
    ] {
        validate_list(label, values, true, &entry.id, issues);
    }
    validate_list(
        "does_not_authorize",
        &entry.does_not_authorize,
        entry.kind == EntryKind::Operation,
        &entry.id,
        issues,
    );
    validate_list(
        "session_surfaces",
        &entry.session_surfaces,
        entry.kind == EntryKind::Operation,
        &entry.id,
        issues,
    );
}

fn validate_interfaces(entry: &EntryDocument, issues: &mut Vec<Issue>) {
    let mut interface_labels = BTreeSet::new();
    for interface in &entry.interfaces {
        validate_text("interface label", &interface.label, issues, Some(&entry.id));
        validate_text(
            "interface invocation",
            &interface.invocation,
            issues,
            Some(&entry.id),
        );
        if !interface_labels.insert(normalized(&interface.label)) {
            issues.push(
                Issue::new(
                    "duplicate_interface",
                    format!("entry {} repeats an interface label", entry.id),
                )
                .entry(&entry.id),
            );
        }
    }
}

fn validate_dependencies(entry: &EntryDocument, issues: &mut Vec<Issue>) {
    let mut dependency_ids = BTreeSet::new();
    for dependency in &entry.dependencies {
        if !valid_entry_id(&dependency.id) {
            issues.push(
                Issue::new(
                    "invalid_dependency",
                    format!(
                        "entry {} has invalid dependency {}",
                        entry.id, dependency.id
                    ),
                )
                .entry(&entry.id),
            );
        }
        if dependency.min_contract == 0
            || dependency.max_contract_exclusive <= dependency.min_contract
        {
            issues.push(
                Issue::new(
                    "invalid_dependency_range",
                    format!(
                        "entry {} has invalid dependency range for {}",
                        entry.id, dependency.id
                    ),
                )
                .entry(&entry.id),
            );
        }
        if !dependency_ids.insert(dependency.id.clone()) {
            issues.push(
                Issue::new(
                    "duplicate_dependency",
                    format!("entry {} repeats dependency {}", entry.id, dependency.id),
                )
                .entry(&entry.id),
            );
        }
    }
}

fn validate_entry_kind(entry: &EntryDocument, issues: &mut Vec<Issue>) {
    match entry.kind {
        EntryKind::Capability => {
            if entry.interfaces.is_empty() {
                issues.push(
                    Issue::new(
                        "missing_interface",
                        format!("capability {} must declare an interface", entry.id),
                    )
                    .entry(&entry.id),
                );
            }
            if entry.runtime.is_some()
                || entry.automation.is_some()
                || !entry.steps.is_empty()
                || !entry.checkpoints.is_empty()
                || !entry.adaptation.is_empty()
                || !entry.stop_when.is_empty()
                || !entry.session_surfaces.is_empty()
            {
                issues.push(
                    Issue::new(
                        "unexpected_operation_fields",
                        format!("capability {} declares operation-only fields", entry.id),
                    )
                    .entry(&entry.id),
                );
            }
        }
        EntryKind::Operation => {
            if entry.runtime.as_deref() != Some("interactive_agent") {
                issues.push(
                    Issue::new(
                        "invalid_operation_runtime",
                        format!("operation {} runtime must be interactive_agent", entry.id),
                    )
                    .entry(&entry.id),
                );
            }
            if entry.automation.as_deref() != Some("none") {
                issues.push(
                    Issue::new(
                        "invalid_operation_automation",
                        format!("operation {} automation must be none", entry.id),
                    )
                    .entry(&entry.id),
                );
            }
            for (label, values) in [
                ("steps", entry.steps.as_slice()),
                ("checkpoints", entry.checkpoints.as_slice()),
                ("adaptation", entry.adaptation.as_slice()),
                ("stop_when", entry.stop_when.as_slice()),
            ] {
                validate_list(label, values, true, &entry.id, issues);
            }
        }
    }
}

fn validate_text(label: &str, value: &str, issues: &mut Vec<Issue>, entry: Option<&str>) {
    let issue = if value.trim().is_empty() {
        Some(Issue::new(
            "blank_field",
            format!("{label} must not be blank"),
        ))
    } else if value.len() > MAX_FIELD_BYTES {
        Some(Issue::new(
            "field_too_large",
            format!("{label} exceeds {MAX_FIELD_BYTES} bytes"),
        ))
    } else {
        value
            .chars()
            .find(|character| character.is_control())
            .map(|character| {
                Issue::new(
                    "invalid_control_character",
                    format!(
                        "{label} contains control character U+{:04X}",
                        u32::from(character)
                    ),
                )
            })
    };
    if let Some(mut issue) = issue {
        if let Some(id) = entry {
            issue.entry = Some(id.to_owned());
        }
        issues.push(issue);
    }
}

fn validate_list(
    label: &str,
    values: &[String],
    required: bool,
    entry: &str,
    issues: &mut Vec<Issue>,
) {
    if required && values.is_empty() {
        issues.push(Issue::new("empty_field", format!("{label} must not be empty")).entry(entry));
        return;
    }
    let mut unique = BTreeSet::new();
    for value in values {
        validate_text(label, value, issues, Some(entry));
        if !unique.insert(normalized(value)) {
            issues.push(
                Issue::new(
                    "duplicate_value",
                    format!("entry {entry} repeats a value in {label}"),
                )
                .entry(entry),
            );
        }
    }
}

fn read_bundle_file(
    root: &Path,
    relative: &str,
    maximum_bytes: u64,
    error_code: &str,
) -> Result<String, Vec<Issue>> {
    let relative_path = Path::new(relative);
    if !safe_relative_path(relative_path) {
        return Err(vec![
            Issue::new(
                "unsafe_bundle_path",
                format!("bundle path must be a safe relative path: {relative}"),
            )
            .path(relative),
        ]);
    }
    let mut current = root.to_path_buf();
    for component in relative_path {
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            vec![
                Issue::new(error_code, format!("unable to read {relative}: {error}"))
                    .path(relative),
            ]
        })?;
        if metadata.file_type().is_symlink() {
            return Err(vec![
                Issue::new(
                    "bundle_symlink_rejected",
                    format!("symbolic links are not allowed inside a bundle: {relative}"),
                )
                .path(relative),
            ]);
        }
    }
    let canonical = fs::canonicalize(&current).map_err(|error| {
        vec![
            Issue::new(error_code, format!("unable to resolve {relative}: {error}")).path(relative),
        ]
    })?;
    if !canonical.starts_with(root) {
        return Err(vec![
            Issue::new(
                "bundle_path_escape",
                format!("bundle path resolves outside its root: {relative}"),
            )
            .path(relative),
        ]);
    }
    let metadata = fs::metadata(&canonical).map_err(|error| {
        vec![
            Issue::new(error_code, format!("unable to inspect {relative}: {error}")).path(relative),
        ]
    })?;
    if !metadata.is_file() {
        return Err(vec![
            Issue::new(error_code, format!("bundle path is not a file: {relative}")).path(relative),
        ]);
    }
    if metadata.len() > maximum_bytes {
        return Err(vec![
            Issue::new(
                "bundle_file_too_large",
                format!("bundle file exceeds {maximum_bytes} bytes: {relative}"),
            )
            .path(relative),
        ]);
    }
    let file = fs::File::open(&canonical).map_err(|error| {
        vec![Issue::new(error_code, format!("unable to read {relative}: {error}")).path(relative)]
    })?;
    let mut bytes = Vec::new();
    file.take(maximum_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            vec![
                Issue::new(error_code, format!("unable to read {relative}: {error}"))
                    .path(relative),
            ]
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum_bytes {
        return Err(vec![
            Issue::new(
                "bundle_file_too_large",
                format!("bundle file exceeds {maximum_bytes} bytes: {relative}"),
            )
            .path(relative),
        ]);
    }
    String::from_utf8(bytes).map_err(|error| {
        vec![
            Issue::new(
                "bundle_file_not_utf8",
                format!("bundle file is not UTF-8: {relative}: {error}"),
            )
            .path(relative),
        ]
    })
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn path_has_parent(value: &str, parent: &str, extension: &str) -> bool {
    let path = Path::new(value);
    safe_relative_path(path)
        && path.components().count() == 2
        && path.parent() == Some(Path::new(parent))
        && path.extension().and_then(|value| value.to_str()) == Some(extension)
}

fn sha256(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn valid_slug(value: &str) -> bool {
    let mut characters = value.chars();
    matches!(characters.next(), Some('a'..='z'))
        && characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

fn valid_entry_id(value: &str) -> bool {
    let mut segments = value.split('.');
    let Some(first) = segments.next() else {
        return false;
    };
    valid_slug(first) && segments.clone().next().is_some() && segments.all(valid_slug)
}

fn normalized(value: &str) -> String {
    value.nfkc().flat_map(char::to_lowercase).collect()
}

fn attach_provider(issues: &mut [Issue], provider: &str) {
    for issue in issues {
        if issue.provider.is_none() {
            issue.provider = Some(provider.to_owned());
        }
    }
}

fn exclude_duplicate_entries(providers: &mut Vec<ProviderBundle>, issues: &mut Vec<Issue>) {
    let mut owners: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (provider_index, provider) in providers.iter().enumerate() {
        for entry in &provider.entries {
            owners
                .entry(entry.document.id.clone())
                .or_default()
                .push(provider_index);
        }
    }
    let mut excluded = BTreeSet::new();
    for (id, provider_indexes) in owners {
        if provider_indexes.len() > 1 {
            for provider_index in provider_indexes {
                excluded.insert(provider_index);
                let provider = &providers[provider_index].identity.id;
                issues.push(
                    Issue::new(
                        "duplicate_entry_id",
                        format!("entry ID {id} is defined by multiple providers"),
                    )
                    .provider(provider)
                    .entry(&id),
                );
            }
        }
    }
    let mut index = 0;
    providers.retain(|_| {
        let retain = !excluded.contains(&index);
        index += 1;
        retain
    });
}

fn calculate_dependencies(providers: &mut [ProviderBundle], issues: &mut Vec<Issue>) {
    let catalog = dependency_catalog(providers);
    let adjacency = dependency_adjacency(providers, &catalog);
    let cycle_nodes = dependency_cycle_nodes(&adjacency);
    let compatibility = transitive_compatibility(providers, &catalog, &cycle_nodes);

    for provider in providers {
        for entry in &mut provider.entries {
            let mut statuses = Vec::with_capacity(entry.document.dependencies.len());
            for dependency in &entry.document.dependencies {
                let installed_contract = catalog.get(&dependency.id).copied();
                let state = dependency_state(
                    &entry.document.id,
                    dependency,
                    installed_contract,
                    &compatibility,
                    &cycle_nodes,
                    &adjacency,
                );
                if state != DependencyState::Compatible {
                    let (code, message) =
                        dependency_issue(&entry.document.id, dependency, installed_contract, state);
                    issues.push(
                        Issue::new(code, message)
                            .provider(&provider.identity.id)
                            .entry(&entry.document.id),
                    );
                }
                statuses.push(DependencyStatus {
                    id: dependency.id.clone(),
                    min_contract: dependency.min_contract,
                    max_contract_exclusive: dependency.max_contract_exclusive,
                    installed_contract,
                    state,
                });
            }
            entry.compatible = compatibility
                .get(&entry.document.id)
                .copied()
                .unwrap_or(false);
            entry.dependency_statuses = statuses;
        }
    }
}

fn dependency_catalog(providers: &[ProviderBundle]) -> BTreeMap<String, u32> {
    providers
        .iter()
        .flat_map(|provider| {
            provider
                .entries
                .iter()
                .map(|entry| (entry.document.id.clone(), entry.document.contract_version))
        })
        .collect()
}

fn dependency_adjacency(
    providers: &[ProviderBundle],
    catalog: &BTreeMap<String, u32>,
) -> BTreeMap<String, Vec<String>> {
    providers
        .iter()
        .flat_map(|provider| provider.entries.iter())
        .map(|entry| {
            (
                entry.document.id.clone(),
                entry
                    .document
                    .dependencies
                    .iter()
                    .filter(|dependency| catalog.contains_key(&dependency.id))
                    .map(|dependency| dependency.id.clone())
                    .collect(),
            )
        })
        .collect()
}

fn transitive_compatibility(
    providers: &[ProviderBundle],
    catalog: &BTreeMap<String, u32>,
    cycle_nodes: &BTreeSet<String>,
) -> BTreeMap<String, bool> {
    let entries: Vec<_> = providers
        .iter()
        .flat_map(|provider| provider.entries.iter())
        .collect();
    let mut compatibility: BTreeMap<String, bool> = entries
        .iter()
        .map(|entry| {
            let directly_compatible = !cycle_nodes.contains(&entry.document.id)
                && entry.document.dependencies.iter().all(|dependency| {
                    catalog.get(&dependency.id).is_some_and(|version| {
                        *version >= dependency.min_contract
                            && *version < dependency.max_contract_exclusive
                    })
                });
            (entry.document.id.clone(), directly_compatible)
        })
        .collect();
    loop {
        let newly_unavailable: Vec<_> = entries
            .iter()
            .filter(|entry| {
                compatibility
                    .get(&entry.document.id)
                    .copied()
                    .unwrap_or(false)
                    && entry.document.dependencies.iter().any(|dependency| {
                        !compatibility.get(&dependency.id).copied().unwrap_or(false)
                    })
            })
            .map(|entry| entry.document.id.clone())
            .collect();
        if newly_unavailable.is_empty() {
            break;
        }
        for id in newly_unavailable {
            compatibility.insert(id, false);
        }
    }
    compatibility
}

fn dependency_state(
    entry_id: &str,
    dependency: &crate::model::Dependency,
    installed_contract: Option<u32>,
    compatibility: &BTreeMap<String, bool>,
    cycle_nodes: &BTreeSet<String>,
    adjacency: &BTreeMap<String, Vec<String>>,
) -> DependencyState {
    if cycle_nodes.contains(entry_id)
        && cycle_nodes.contains(&dependency.id)
        && dependency_edge_is_cyclic(entry_id, &dependency.id, adjacency)
    {
        return DependencyState::Cycle;
    }
    match installed_contract {
        None => DependencyState::Missing,
        Some(version)
            if version < dependency.min_contract
                || version >= dependency.max_contract_exclusive =>
        {
            DependencyState::Incompatible
        }
        Some(_) if !compatibility.get(&dependency.id).copied().unwrap_or(false) => {
            DependencyState::Unavailable
        }
        Some(_) => DependencyState::Compatible,
    }
}

fn dependency_edge_is_cyclic(
    entry_id: &str,
    dependency_id: &str,
    adjacency: &BTreeMap<String, Vec<String>>,
) -> bool {
    let mut pending = vec![dependency_id];
    let mut visited = BTreeSet::new();
    while let Some(node) = pending.pop() {
        if node == entry_id {
            return true;
        }
        if !visited.insert(node) {
            continue;
        }
        if let Some(dependencies) = adjacency.get(node) {
            pending.extend(dependencies.iter().map(String::as_str));
        }
    }
    false
}

fn dependency_issue(
    entry_id: &str,
    dependency: &crate::model::Dependency,
    installed_contract: Option<u32>,
    state: DependencyState,
) -> (&'static str, String) {
    match state {
        DependencyState::Missing => (
            "missing_dependency",
            format!(
                "entry {entry_id} requires missing capability {}",
                dependency.id
            ),
        ),
        DependencyState::Incompatible => (
            "incompatible_dependency",
            format!(
                "entry {entry_id} requires {} contract >= {}, < {}; installed contract is {}",
                dependency.id,
                dependency.min_contract,
                dependency.max_contract_exclusive,
                installed_contract.unwrap_or_default()
            ),
        ),
        DependencyState::Unavailable => (
            "unavailable_dependency",
            format!(
                "entry {entry_id} requires installed capability {}, but that capability's own dependencies are unavailable",
                dependency.id
            ),
        ),
        DependencyState::Cycle => (
            "dependency_cycle",
            format!(
                "entry {entry_id} participates in a dependency cycle through {}",
                dependency.id
            ),
        ),
        DependencyState::Compatible => unreachable!(),
    }
}

fn dependency_cycle_nodes(adjacency: &BTreeMap<String, Vec<String>>) -> BTreeSet<String> {
    fn visit(
        node: &str,
        adjacency: &BTreeMap<String, Vec<String>>,
        states: &mut BTreeMap<String, u8>,
        stack: &mut Vec<String>,
        cycles: &mut BTreeSet<String>,
    ) {
        states.insert(node.to_owned(), 1);
        stack.push(node.to_owned());
        if let Some(neighbors) = adjacency.get(node) {
            for neighbor in neighbors {
                match states.get(neighbor).copied().unwrap_or(0) {
                    0 => visit(neighbor, adjacency, states, stack, cycles),
                    1 => {
                        if let Some(position) = stack.iter().position(|value| value == neighbor) {
                            cycles.extend(stack[position..].iter().cloned());
                        }
                    }
                    _ => {}
                }
            }
        }
        let _ = stack.pop();
        states.insert(node.to_owned(), 2);
    }

    let mut states = BTreeMap::new();
    let mut stack = Vec::new();
    let mut cycles = BTreeSet::new();
    for node in adjacency.keys() {
        if states.get(node).copied().unwrap_or(0) == 0 {
            visit(node, adjacency, &mut states, &mut stack, &mut cycles);
        }
    }
    cycles
}

#[cfg(test)]
mod tests {
    use super::{safe_relative_path, valid_entry_id, valid_slug};
    use std::path::Path;

    #[test]
    fn identifiers_are_deliberately_narrow() {
        assert!(valid_slug("career-ops"));
        assert!(!valid_slug("Career"));
        assert!(valid_entry_id("todo.concern.capture-and-route"));
        assert!(!valid_entry_id("todo"));
        assert!(!valid_entry_id("todo.Concern"));
    }

    #[test]
    fn bundle_paths_cannot_escape() {
        assert!(safe_relative_path(Path::new("entries/item.json")));
        assert!(!safe_relative_path(Path::new("../item.json")));
        assert!(!safe_relative_path(Path::new("/tmp/item.json")));
    }
}
