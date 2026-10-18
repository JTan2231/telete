//! Provider-owned Chancery documents, partial introduction views, and CLI output.
//!
//! Decoding establishes the selected shape, not full bundle validity, installed
//! presence, runtime readiness, or authority to invoke a represented interface.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use crate::model::{
    ClaimStatus, Dependency, DependencyState, DependencyStatus, EntryDocument, EntryKind,
    EntryPromise, Interface, InventoryCompleteness, InventoryScope, Issue,
    LEGACY_PROVIDER_SCHEMA_VERSION, Mode, OUTPUT_SCHEMA_VERSION, PREVIOUS_PROVIDER_SCHEMA_VERSION,
    PROVIDER_SCHEMA_VERSION, PromiseClaim, PromiseFacet, ProviderIdentity, ProviderManifest,
    ProviderPromiseScope, RelianceClaim, RelianceKind, Support, V2_PROVIDER_SCHEMA_VERSION,
};

/// Only the provider fields needed to recognize an introduction. Unrelated
/// fields are deliberately ignored, including fields unknown to this reader.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderIntroduction {
    pub schema_version: u32,
    pub provider: IntroductionIdentity,
    pub entries: Vec<String>,
}

/// A partial identity view; unlike full bundle validation it permits extra fields.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct IntroductionIdentity {
    pub id: String,
    pub name: String,
    pub release: String,
}

/// The indexed identity and manual pointer, without other contract fields.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct EntryIntroduction {
    pub id: String,
    pub contract_version: u32,
    pub manual: String,
}

impl ProviderIntroduction {
    /// Decode the introduction fields without validating the complete bundle.
    ///
    /// # Errors
    /// Returns a JSON error if the selected document shape cannot be decoded.
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

impl EntryIntroduction {
    /// Decode the introduction fields without interpreting the entry promise.
    ///
    /// # Errors
    /// Returns a JSON error if the selected document shape cannot be decoded.
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

impl ProviderManifest {
    /// Decode the complete manifest while preserving absent versus null fields.
    ///
    /// # Errors
    /// Returns a JSON error if the selected document shape cannot be decoded.
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        let present = value
            .as_object()
            .is_some_and(|v| v.contains_key("promise_scope"));
        let overview_present = value
            .as_object()
            .is_some_and(|v| v.contains_key("overview"));
        let mut manifest: Self = serde_json::from_value(value)?;
        manifest.promise_scope_present = present;
        manifest.overview_present = overview_present;
        Ok(manifest)
    }
}

impl EntryDocument {
    /// Decode an entry using the owning bundle's schema, including schema-one
    /// legacy routing fields and absent-versus-null promise preservation.
    ///
    /// # Errors
    /// Returns a JSON error if the selected document shape cannot be decoded.
    pub fn decode(text: &str, schema_version: u32) -> Result<Self, serde_json::Error> {
        let mut value: serde_json::Value = serde_json::from_str(text)?;
        let present = value.as_object().is_some_and(|v| v.contains_key("promise"));
        if schema_version == LEGACY_PROVIDER_SCHEMA_VERSION
            && let Some(object) = value.as_object_mut()
        {
            object.remove("routable");
            object.remove("routing");
        }
        let mut entry: Self = serde_json::from_value(value)?;
        entry.promise_present = present;
        Ok(entry)
    }
}

/// The CLI envelope shared by product, list, show, resolve, doctor, and validate.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Output<T> {
    pub schema_version: u32,
    pub ok: bool,
    pub data: T,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorOutput {
    pub schema_version: u32,
    pub ok: bool,
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogEntry {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub kind: EntryKind,
    pub mode: Mode,
    /// Only exceptions to the catalog defaults are repeated on a card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub support: Option<Support>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogDefaults {
    pub support: Support,
    pub availability: String,
    pub compatibility: String,
    pub readiness: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListResult {
    pub defaults: CatalogDefaults,
    pub entries: Vec<CatalogEntry>,
    pub issues: Vec<Issue>,
}

/// Installed product overview and its own catalog; this is not a readiness probe.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProductResult {
    pub provider: ProviderIdentity,
    pub provider_schema_version: u32,
    pub promise_scope: Option<ProviderPromiseScope>,
    pub overview_status: String,
    pub overview: Option<String>,
    pub defaults: CatalogDefaults,
    pub entries: Vec<CatalogEntry>,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EntryIdentity {
    pub id: String,
    pub title: String,
    pub kind: EntryKind,
    pub mode: Mode,
    pub contract_version: u32,
    pub support: Support,
}

impl From<&EntryDocument> for EntryIdentity {
    fn from(entry: &EntryDocument) -> Self {
        Self {
            id: entry.id.clone(),
            title: entry.title.clone(),
            kind: entry.kind,
            mode: entry.mode,
            contract_version: entry.contract_version,
            support: entry.support,
        }
    }
}

/// One self-contained authored operating manual, without duplicate authoring representations.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShowResult {
    pub provider: ProviderIdentity,
    pub entry: EntryIdentity,
    pub availability: String,
    pub compatibility: String,
    pub readiness: String,
    pub dependency_statuses: Vec<DependencyStatus>,
    pub manual: String,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FullShowResult {
    pub provider: ProviderIdentity,
    pub entry: EntryDocument,
    pub availability: String,
    pub compatibility: String,
    pub readiness: String,
    pub dependency_statuses: Vec<DependencyStatus>,
    pub manual: String,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResolutionGap {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facet: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractRequirement {
    pub min_contract: Option<u32>,
    pub max_contract_exclusive: Option<u32>,
    pub satisfied: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FacetRequirements {
    pub required: Vec<String>,
    pub unsatisfied: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FacetCoverage {
    pub state: String,
    pub claim_statuses: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileBasis {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractBasis {
    pub provider_manifest: FileBasis,
    pub entry_contract: FileBasis,
    pub manual: FileBasis,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractDossier {
    pub provider: ProviderIdentity,
    pub provider_schema_version: u32,
    pub provider_promise_scope: Option<ProviderPromiseScope>,
    pub entry: EntryDocument,
    pub facet_coverage: BTreeMap<String, FacetCoverage>,
    pub availability: String,
    pub compatibility: String,
    pub readiness: String,
    pub dependency_statuses: Vec<DependencyStatus>,
    pub manual: String,
    pub basis: ContractBasis,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResolveResult {
    pub requested_id: String,
    pub status: String,
    pub contract_requirement: ContractRequirement,
    pub facet_requirements: FacetRequirements,
    pub declaration_status: String,
    pub dependency_closure_status: String,
    pub readiness: String,
    pub root: ContractDossier,
    pub dependency_closure: Vec<ContractDossier>,
    pub gaps: Vec<ResolutionGap>,
    pub issues: Vec<Issue>,
}

/// Outcome-only resolution; no contract bodies or basis dossier is implied.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResolveSummary {
    pub requested_id: String,
    pub status: String,
    pub contract_requirement: ContractRequirement,
    pub facet_requirements: FacetRequirements,
    pub declaration_status: String,
    pub dependency_closure_status: String,
    pub readiness: String,
    pub gaps: Vec<ResolutionGap>,
    pub issues: Vec<Issue>,
}

impl From<&ResolveResult> for ResolveSummary {
    fn from(result: &ResolveResult) -> Self {
        Self {
            requested_id: result.requested_id.clone(),
            status: result.status.clone(),
            contract_requirement: result.contract_requirement.clone(),
            facet_requirements: result.facet_requirements.clone(),
            declaration_status: result.declaration_status.clone(),
            dependency_closure_status: result.dependency_closure_status.clone(),
            readiness: result.readiness.clone(),
            gaps: result.gaps.clone(),
            issues: result.issues.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub release: String,
    pub root: PathBuf,
    pub entries: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RegistryCounts {
    pub scanned_providers: usize,
    pub valid_providers: usize,
    pub excluded_providers: usize,
    pub entries: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DoctorResult {
    pub valid: bool,
    pub registry: PathBuf,
    pub providers: Vec<ProviderSummary>,
    pub counts: RegistryCounts,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ValidateResult {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderIdentity>,
    pub bundle: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<usize>,
    pub external_dependencies: String,
    pub issues: Vec<Issue>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot invoke Chancery: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid Chancery response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{code}: {message}")]
    Provider { code: String, message: String },
    #[error("invalid Chancery response: {0}")]
    Protocol(String),
}

/// Typed access to an explicitly selected Chancery executable and registry.
/// Reports retain `ok = false` domain outcomes such as unresolved promises or
/// invalid bundles; invocation failures are returned separately.
#[derive(Debug, Clone)]
pub struct Client {
    executable: PathBuf,
    registry: Option<PathBuf>,
}

#[allow(clippy::missing_errors_doc)]
impl Client {
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            registry: None,
        }
    }

    #[must_use]
    pub fn with_registry(mut self, registry: impl Into<PathBuf>) -> Self {
        self.registry = Some(registry.into());
        self
    }

    fn invoke<T: serde::de::DeserializeOwned>(
        &self,
        args: &[std::ffi::OsString],
    ) -> Result<Output<T>, ClientError> {
        let mut command = std::process::Command::new(&self.executable);
        command.env("CHANCERY_USAGE_INTERNAL", "1");
        command.arg("--json");
        if let Some(registry) = &self.registry {
            command.arg("--registry").arg(registry);
        }
        let result = command.args(args).output()?;
        if result.stdout.is_empty() {
            let error: ErrorOutput = serde_json::from_slice(&result.stderr)?;
            if error.schema_version != OUTPUT_SCHEMA_VERSION || error.ok {
                return Err(ClientError::Protocol("unsupported error envelope".into()));
            }
            return Err(ClientError::Provider {
                code: error.error.code,
                message: error.error.message,
            });
        }
        let report: Output<T> = serde_json::from_slice(&result.stdout)?;
        if report.schema_version != OUTPUT_SCHEMA_VERSION || report.ok != result.status.success() {
            return Err(ClientError::Protocol(
                "unsupported or inconsistent report envelope".into(),
            ));
        }
        Ok(report)
    }

    pub fn list(
        &self,
        mode: Option<Mode>,
        kind: Option<EntryKind>,
    ) -> Result<Output<ListResult>, ClientError> {
        self.list_filtered(None, mode, kind)
    }

    pub fn list_provider(
        &self,
        provider: &str,
        mode: Option<Mode>,
        kind: Option<EntryKind>,
    ) -> Result<Output<ListResult>, ClientError> {
        self.list_filtered(Some(provider), mode, kind)
    }

    fn list_filtered(
        &self,
        provider: Option<&str>,
        mode: Option<Mode>,
        kind: Option<EntryKind>,
    ) -> Result<Output<ListResult>, ClientError> {
        let mut args: Vec<std::ffi::OsString> = vec!["list".into()];
        if let Some(provider) = provider {
            args.extend(["--provider".into(), provider.into()]);
        }
        if let Some(mode) = mode {
            args.extend(["--mode".into(), mode.as_str().into()]);
        }
        if let Some(kind) = kind {
            args.extend(["--kind".into(), kind.as_str().into()]);
        }
        self.invoke(&args)
    }

    pub fn product(&self, provider: &str) -> Result<Output<ProductResult>, ClientError> {
        self.invoke(&["product".into(), provider.into()])
    }

    pub fn show(&self, id: &str) -> Result<Output<ShowResult>, ClientError> {
        self.invoke(&["show".into(), id.into()])
    }

    pub fn show_full(&self, id: &str) -> Result<Output<FullShowResult>, ClientError> {
        self.invoke(&["show".into(), id.into(), "--full".into()])
    }

    pub fn resolve_summary(&self, id: &str) -> Result<Output<ResolveSummary>, ClientError> {
        self.invoke(&["resolve".into(), id.into(), "--summary".into()])
    }

    pub fn resolve(
        &self,
        id: &str,
        min_contract: Option<u32>,
        max_contract_exclusive: Option<u32>,
        require: &[PromiseFacet],
    ) -> Result<Output<ResolveResult>, ClientError> {
        let mut args: Vec<std::ffi::OsString> = vec!["resolve".into(), id.into()];
        if let Some(min) = min_contract {
            args.extend(["--min-contract".into(), min.to_string().into()]);
        }
        if let Some(max) = max_contract_exclusive {
            args.extend(["--max-contract-exclusive".into(), max.to_string().into()]);
        }
        for facet in require {
            args.extend(["--require".into(), facet.as_str().into()]);
        }
        self.invoke(&args)
    }

    pub fn doctor(&self) -> Result<Output<DoctorResult>, ClientError> {
        self.invoke(&["doctor".into()])
    }

    pub fn validate(
        &self,
        bundle: &std::path::Path,
    ) -> Result<Output<ValidateResult>, ClientError> {
        self.invoke(&["validate".into(), bundle.as_os_str().to_owned()])
    }
}
