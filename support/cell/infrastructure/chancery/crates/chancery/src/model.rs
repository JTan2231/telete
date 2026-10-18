use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const OUTPUT_SCHEMA_VERSION: u32 = 3;
pub const PROVIDER_SCHEMA_VERSION: u32 = 4;
pub const PREVIOUS_PROVIDER_SCHEMA_VERSION: u32 = 3;
pub const V2_PROVIDER_SCHEMA_VERSION: u32 = 2;
pub const LEGACY_PROVIDER_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderManifest {
    pub schema_version: u32,
    pub provider: ProviderIdentity,
    #[serde(default)]
    pub promise_scope: Option<ProviderPromiseScope>,
    #[serde(skip)]
    pub(crate) promise_scope_present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(skip)]
    pub(crate) overview_present: bool,
    pub entries: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPromiseScope {
    pub authoritative_for: Vec<String>,
    pub not_authoritative_for: Vec<String>,
    pub inventory: InventoryScope,
    pub shared_access_and_trust: Vec<String>,
    pub shared_privacy_and_retention: Vec<String>,
    pub compatibility_and_retirement: Vec<String>,
    pub operational_limits: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryScope {
    pub covers: Vec<String>,
    pub completeness: InventoryCompleteness,
    pub excludes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InventoryCompleteness {
    Complete,
    Partial,
}

impl InventoryCompleteness {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderIdentity {
    pub id: String,
    pub name: String,
    pub release: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Capability,
    Operation,
}

impl EntryKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Capability => "capability",
            Self::Operation => "operation",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Use,
    Operate,
    Develop,
}

impl Mode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::Operate => "operate",
            Self::Develop => "develop",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    Deprecated,
}

impl Support {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Deprecated => "deprecated",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    pub label: String,
    pub invocation: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub id: String,
    pub min_contract: u32,
    pub max_contract_exclusive: u32,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    Declared,
    Unsupported,
    Unspecified,
    NotApplicable,
}

impl ClaimStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Unsupported => "unsupported",
            Self::Unspecified => "unspecified",
            Self::NotApplicable => "not_applicable",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseClaim {
    pub status: ClaimStatus,
    pub statement: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RelianceKind {
    Data,
    Control,
    Authority,
    Readiness,
    External,
}

impl RelianceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Control => "control",
            Self::Authority => "authority",
            Self::Readiness => "readiness",
            Self::External => "external",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelianceClaim {
    pub status: ClaimStatus,
    pub statement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RelianceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPromise {
    pub consumers: Vec<PromiseClaim>,
    pub preconditions: Vec<PromiseClaim>,
    pub inputs: Vec<PromiseClaim>,
    pub outputs: Vec<PromiseClaim>,
    pub data_semantics: Vec<PromiseClaim>,
    pub identity_and_units: Vec<PromiseClaim>,
    pub completeness_and_freshness: Vec<PromiseClaim>,
    pub access: Vec<PromiseClaim>,
    pub lifecycle_and_consistency: Vec<PromiseClaim>,
    pub operational_limits: Vec<PromiseClaim>,
    pub compatibility_and_evolution: Vec<PromiseClaim>,
    pub reliances: Vec<RelianceClaim>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum PromiseFacet {
    Applicability,
    Outcome,
    Consumers,
    Preconditions,
    Interfaces,
    Inputs,
    Outputs,
    DataSemantics,
    IdentityAndUnits,
    CompletenessAndFreshness,
    Effects,
    Authority,
    Access,
    LifecycleAndConsistency,
    Success,
    FailureAndRecovery,
    Privacy,
    OperationalLimits,
    CompatibilityAndEvolution,
    Dependencies,
    Reliances,
    Exclusions,
}

impl PromiseFacet {
    pub const ALL: [Self; 22] = [
        Self::Applicability,
        Self::Outcome,
        Self::Consumers,
        Self::Preconditions,
        Self::Interfaces,
        Self::Inputs,
        Self::Outputs,
        Self::DataSemantics,
        Self::IdentityAndUnits,
        Self::CompletenessAndFreshness,
        Self::Effects,
        Self::Authority,
        Self::Access,
        Self::LifecycleAndConsistency,
        Self::Success,
        Self::FailureAndRecovery,
        Self::Privacy,
        Self::OperationalLimits,
        Self::CompatibilityAndEvolution,
        Self::Dependencies,
        Self::Reliances,
        Self::Exclusions,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applicability => "applicability",
            Self::Outcome => "outcome",
            Self::Consumers => "consumers",
            Self::Preconditions => "preconditions",
            Self::Interfaces => "interfaces",
            Self::Inputs => "inputs",
            Self::Outputs => "outputs",
            Self::DataSemantics => "data_semantics",
            Self::IdentityAndUnits => "identity_and_units",
            Self::CompletenessAndFreshness => "completeness_and_freshness",
            Self::Effects => "effects",
            Self::Authority => "authority",
            Self::Access => "access",
            Self::LifecycleAndConsistency => "lifecycle_and_consistency",
            Self::Success => "success",
            Self::FailureAndRecovery => "failure_and_recovery",
            Self::Privacy => "privacy",
            Self::OperationalLimits => "operational_limits",
            Self::CompatibilityAndEvolution => "compatibility_and_evolution",
            Self::Dependencies => "dependencies",
            Self::Reliances => "reliances",
            Self::Exclusions => "exclusions",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntryDocument {
    pub id: String,
    pub contract_version: u32,
    pub kind: EntryKind,
    pub mode: Mode,
    pub support: Support,
    pub title: String,
    pub summary: String,
    pub use_when: Vec<String>,
    pub do_not_use_when: Vec<String>,
    pub outcome: String,
    pub effects: Vec<String>,
    pub authority: Vec<String>,
    pub success: Vec<String>,
    pub failure_and_recovery: Vec<String>,
    pub privacy: Vec<String>,
    #[serde(default)]
    pub interfaces: Vec<Interface>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub promise: Option<EntryPromise>,
    #[serde(skip)]
    pub(crate) promise_present: bool,
    #[serde(default)]
    pub session_surfaces: Vec<String>,
    #[serde(default)]
    pub does_not_authorize: Vec<String>,
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub automation: Option<String>,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub checkpoints: Vec<String>,
    #[serde(default)]
    pub adaptation: Vec<String>,
    #[serde(default)]
    pub stop_when: Vec<String>,
    pub manual: String,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedEntry {
    pub(crate) document: EntryDocument,
    pub(crate) source_path: String,
    pub(crate) source_sha256: String,
    pub(crate) manual_text: String,
    pub(crate) manual_sha256: String,
    pub(crate) dependency_statuses: Vec<DependencyStatus>,
    pub(crate) compatible: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DependencyStatus {
    pub id: String,
    pub min_contract: u32,
    pub max_contract_exclusive: u32,
    pub installed_contract: Option<u32>,
    pub state: DependencyState,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyState {
    Compatible,
    Missing,
    Incompatible,
    Unavailable,
    Cycle,
}

impl DependencyState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compatible => "compatible",
            Self::Missing => "missing",
            Self::Incompatible => "incompatible",
            Self::Unavailable => "unavailable",
            Self::Cycle => "cycle",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderBundle {
    pub(crate) schema_version: u32,
    pub(crate) identity: ProviderIdentity,
    pub(crate) promise_scope: Option<ProviderPromiseScope>,
    pub(crate) overview_text: Option<String>,
    pub(crate) root: PathBuf,
    pub(crate) manifest_sha256: String,
    pub(crate) entries: Vec<LoadedEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Issue {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Issue {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            provider: None,
            entry: None,
            path: None,
        }
    }

    #[must_use]
    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    #[must_use]
    pub fn entry(mut self, entry: impl Into<String>) -> Self {
        self.entry = Some(entry.into());
        self
    }

    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

#[derive(Debug)]
pub(crate) struct Registry {
    pub(crate) root: PathBuf,
    pub(crate) providers: Vec<ProviderBundle>,
    pub(crate) issues: Vec<Issue>,
    pub(crate) scanned_providers: usize,
}

impl Registry {
    pub fn entries(&self) -> impl Iterator<Item = (&ProviderIdentity, &LoadedEntry)> {
        self.providers.iter().flat_map(|provider| {
            provider
                .entries
                .iter()
                .map(move |entry| (&provider.identity, entry))
        })
    }

    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.providers
            .iter()
            .map(|provider| provider.entries.len())
            .sum()
    }

    pub fn find_entry(&self, id: &str) -> Option<(&ProviderBundle, &LoadedEntry)> {
        self.providers.iter().find_map(|provider| {
            provider
                .entries
                .iter()
                .find(|entry| entry.document.id == id)
                .map(|entry| (provider, entry))
        })
    }
}
