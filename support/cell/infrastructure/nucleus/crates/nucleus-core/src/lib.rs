//! Stable runtime contracts shared by the Nucleus daemon, clients, stores, and
//! harness adapters.
//!
//! These types deliberately describe runtime concerns only. A requester owns
//! prompt construction, dynamic-tool behavior, and the meaning of its domain
//! result. Nucleus owns admission, harness execution, lifecycle, and the exact
//! harness-output observations used by reporting surfaces.

mod quota;
pub use quota::*;

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

/// The only public protocol version currently understood by Nucleus.
pub const PROTOCOL_VERSION_V1: u32 = 1;

/// Invocation policy version that adds unrestricted current-user execution.
pub const INVOCATION_VERSION_V2: u32 = 2;

/// Upper bounds are admission safeguards, not storage-format restrictions.
const MAX_IDENTIFIER_LEN: usize = 255;
const MAX_LABEL_LEN: usize = 1_024;
const MAX_PROGRAM_LEN: usize = 128;

// ---- Primitive runtime values ------------------------------------------------

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

string_id!(JobId);
string_id!(AttemptId);
string_id!(HarnessId);
string_id!(ModelId);
string_id!(SchemaId);
string_id!(ToolCallId);
string_id!(LaunchContextId);

/// An absolute working directory as supplied in a request.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AbsolutePath(pub PathBuf);

impl AbsolutePath {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for AbsolutePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(formatter)
    }
}

/// A positive wall-clock limit. It serializes as the numeric value in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TimeoutSeconds(pub u64);

impl TimeoutSeconds {
    #[must_use]
    pub const fn new(seconds: u64) -> Self {
        Self(seconds)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

// ---- Submission contract -----------------------------------------------------

/// Runtime identity chosen by a requester.
///
/// `(program, id)` is indexed by Nucleus so a domain-specific reporting surface
/// can find its jobs without teaching Nucleus the requester's database schema.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Requester {
    pub program: String,
    pub id: String,
}

/// One variable in an ephemeral launch environment. Variables use strings
/// because the public API is JSON; names must also satisfy the host process
/// environment rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchEnvironmentVariableV1 {
    pub name: String,
    pub value: String,
}

/// A short-lived launch environment uploaded separately from a durable job.
/// Nucleus retains this value only in daemon memory and consumes it when the
/// referenced job is admitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchContextRegistrationV1 {
    pub version: u32,
    pub requester: Requester,
    pub environment: Vec<LaunchEnvironmentVariableV1>,
}

impl LaunchContextRegistrationV1 {
    /// Validate the bounded process environment before sending it.
    ///
    /// # Errors
    ///
    /// Returns every invalid name, duplicate, or size violation.
    pub fn validate(&self) -> Result<(), ValidationError> {
        const MAX_VARIABLES: usize = 4_096;
        const MAX_ENVIRONMENT_BYTES: usize = 1024 * 1024;

        let mut issues = Vec::new();
        check_version("version", self.version, &mut issues);
        check_program("requester.program", &self.requester.program, &mut issues);
        check_nonempty_bounded(
            "requester.id",
            &self.requester.id,
            MAX_IDENTIFIER_LEN,
            &mut issues,
        );
        if self.environment.len() > MAX_VARIABLES {
            issues.push(ValidationIssue::new(
                "environment",
                "too_many",
                format!("environment may contain at most {MAX_VARIABLES} variables"),
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        let mut total = 0_usize;
        for (index, variable) in self.environment.iter().enumerate() {
            total = total
                .saturating_add(variable.name.len())
                .saturating_add(variable.value.len());
            if variable.name.is_empty()
                || variable.name.contains('=')
                || variable.name.contains('\0')
            {
                issues.push(ValidationIssue::new(
                    format!("environment[{index}].name"),
                    "invalid_environment_name",
                    "environment variable names must be nonempty and contain neither '=' nor NUL",
                ));
            } else if !names.insert(variable.name.as_str()) {
                issues.push(ValidationIssue::new(
                    format!("environment[{index}].name"),
                    "duplicate",
                    "environment variable names must be unique",
                ));
            }
            if variable.value.contains('\0') {
                issues.push(ValidationIssue::new(
                    format!("environment[{index}].value"),
                    "invalid_environment_value",
                    "environment variable values must not contain NUL",
                ));
            }
        }
        if total > MAX_ENVIRONMENT_BYTES {
            issues.push(ValidationIssue::new(
                "environment",
                "too_large",
                format!("environment may contain at most {MAX_ENVIRONMENT_BYTES} bytes"),
            ));
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { issues })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceAccess {
    None,
    ReadOnly,
    ReadWrite,
    Unrestricted,
}

/// Built-in model tools controlled by Nucleus independently of requester-owned
/// dynamic tools.
///
/// Local execution covers harness command, inspection, and edit primitives;
/// `workspace_access` remains the authority on what those primitives may do.
/// Web search independently selects live search. Both settings are required so
/// a requester never inherits a harness default accidentally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuiltinToolsV1 {
    pub local_execution: bool,
    pub web_search: bool,
}

/// Stable identity of a requester-owned dynamic tool collection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolsetRef {
    pub provider: String,
    pub name: String,
    pub version: u32,
}

/// The closed, harness-independent invocation policy carried by protocol v1.
/// Policy version two adds unrestricted access; version one retains its modes.
///
/// An adapter either implements every requested semantic exactly or rejects the
/// request. There is no arbitrary argv, environment, or provider-config escape
/// hatch in this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentInvocationV1 {
    pub version: u32,
    pub harness: HarnessId,
    pub model: ModelId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    pub cwd: AbsolutePath,
    pub workspace_access: WorkspaceAccess,
    pub builtin_tools: BuiltinToolsV1,
    pub timeout_seconds: TimeoutSeconds,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolset: Option<ToolsetRef>,
    /// Reference to an ephemeral, daemon-memory-only launch context. The
    /// referenced environment is never included in this durable job request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_context: Option<LaunchContextId>,
}

impl AgentInvocationV1 {
    #[must_use]
    pub fn new(
        harness: impl Into<HarnessId>,
        model: impl Into<ModelId>,
        cwd: AbsolutePath,
        workspace_access: WorkspaceAccess,
        builtin_tools: BuiltinToolsV1,
        timeout_seconds: TimeoutSeconds,
    ) -> Self {
        Self {
            version: if workspace_access == WorkspaceAccess::Unrestricted {
                INVOCATION_VERSION_V2
            } else {
                PROTOCOL_VERSION_V1
            },
            harness: harness.into(),
            model: model.into(),
            reasoning_effort: None,
            cwd,
            workspace_access,
            builtin_tools,
            timeout_seconds,
            toolset: None,
            launch_context: None,
        }
    }
}

/// A complete, runtime-only request to execute one agent invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobRequestV1 {
    pub version: u32,
    pub id: JobId,
    pub label: String,
    pub requester: Requester,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<JobId>,
    pub instructions: String,
    /// Optional developer-priority instructions kept separate from the base
    /// instructions for harnesses that distinguish the two roles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    pub prompt: String,
    pub invocation: AgentInvocationV1,
}

impl JobRequestV1 {
    #[must_use]
    pub fn new(
        id: impl Into<JobId>,
        label: impl Into<String>,
        requester: Requester,
        instructions: impl Into<String>,
        prompt: impl Into<String>,
        invocation: AgentInvocationV1,
    ) -> Self {
        Self {
            version: PROTOCOL_VERSION_V1,
            id: id.into(),
            label: label.into(),
            requester,
            parent: None,
            instructions: instructions.into(),
            developer_instructions: None,
            prompt: prompt.into(),
            invocation,
        }
    }

    /// Validate the stable request contract before harness-specific validation.
    ///
    /// # Errors
    ///
    /// Returns every portable-contract violation found in the request.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let mut issues = Vec::new();

        check_version("version", self.version, &mut issues);
        check_identifier("id", self.id.as_str(), MAX_IDENTIFIER_LEN, &mut issues);
        check_nonempty_bounded("label", &self.label, MAX_LABEL_LEN, &mut issues);
        check_program("requester.program", &self.requester.program, &mut issues);
        check_nonempty_bounded(
            "requester.id",
            &self.requester.id,
            MAX_IDENTIFIER_LEN,
            &mut issues,
        );
        if let Some(parent) = &self.parent {
            check_identifier("parent", parent.as_str(), MAX_IDENTIFIER_LEN, &mut issues);
            if parent == &self.id {
                issues.push(ValidationIssue::new(
                    "parent",
                    "self_parent",
                    "a job cannot name itself as its parent",
                ));
            }
        }
        if self.instructions.trim().is_empty() {
            issues.push(ValidationIssue::new(
                "instructions",
                "empty",
                "instructions must not be empty",
            ));
        }
        if self
            .developer_instructions
            .as_ref()
            .is_some_and(|instructions| instructions.trim().is_empty())
        {
            issues.push(ValidationIssue::new(
                "developerInstructions",
                "empty",
                "developerInstructions must not be empty when supplied",
            ));
        }
        if self.prompt.trim().is_empty() {
            issues.push(ValidationIssue::new(
                "prompt",
                "empty",
                "prompt must not be empty",
            ));
        }
        self.invocation.validate_into(&mut issues);

        if issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { issues })
        }
    }

    /// Digest of the canonical v1 struct serialization used for submission
    /// idempotency. Struct field ordering is fixed by this versioned type.
    ///
    /// # Errors
    ///
    /// Returns an encoding error if a locally constructed path cannot be
    /// represented by the JSON request format.
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        let bytes = serde_json::to_vec(self)?;
        Ok(sha256_digest(&bytes))
    }

    /// Explicitly named alias used at admission call sites.
    ///
    /// # Errors
    ///
    /// Returns the same serialization errors as [`Self::digest`].
    pub fn request_digest(&self) -> Result<String, serde_json::Error> {
        self.digest()
    }
}

impl AgentInvocationV1 {
    /// Validate only the portable invocation contract. Harness-specific
    /// capability validation happens after this succeeds.
    ///
    /// # Errors
    ///
    /// Returns every portable invocation violation found.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let mut issues = Vec::new();
        self.validate_into(&mut issues);
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { issues })
        }
    }

    fn validate_into(&self, issues: &mut Vec<ValidationIssue>) {
        if ![PROTOCOL_VERSION_V1, INVOCATION_VERSION_V2].contains(&self.version) {
            issues.push(ValidationIssue::new(
                "invocation.version",
                "unsupported_version",
                "only invocation versions 1 and 2 are supported",
            ));
        }
        if self.workspace_access == WorkspaceAccess::Unrestricted
            && self.version != INVOCATION_VERSION_V2
        {
            issues.push(ValidationIssue::new(
                "invocation.workspaceAccess",
                "unsupported_setting",
                "unrestricted access requires invocation version 2",
            ));
        }
        check_identifier(
            "invocation.harness",
            self.harness.as_str(),
            MAX_IDENTIFIER_LEN,
            issues,
        );
        check_nonempty_bounded(
            "invocation.model",
            self.model.as_str(),
            MAX_IDENTIFIER_LEN,
            issues,
        );
        if !self.cwd.as_path().is_absolute() {
            issues.push(ValidationIssue::new(
                "invocation.cwd",
                "not_absolute",
                "cwd must be an absolute path",
            ));
        }
        if self.timeout_seconds.get() == 0 {
            issues.push(ValidationIssue::new(
                "invocation.timeoutSeconds",
                "not_positive",
                "timeoutSeconds must be greater than zero",
            ));
        }
        if let Some(toolset) = &self.toolset {
            check_program("invocation.toolset.provider", &toolset.provider, issues);
            check_identifier(
                "invocation.toolset.name",
                &toolset.name,
                MAX_IDENTIFIER_LEN,
                issues,
            );
            if toolset.version == 0 {
                issues.push(ValidationIssue::new(
                    "invocation.toolset.version",
                    "not_positive",
                    "toolset version must be greater than zero",
                ));
            }
        }
        if let Some(context) = &self.launch_context {
            check_identifier(
                "invocation.launchContext",
                context.as_str(),
                MAX_IDENTIFIER_LEN,
                issues,
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationIssue {
    pub field: String,
    pub code: String,
    pub message: String,
}

impl ValidationIssue {
    #[must_use]
    pub fn new(
        field: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            field: field.into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationError {
    pub issues: Vec<ValidationIssue>,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "request validation failed with {} issue(s)",
            self.issues.len()
        )
    }
}

impl std::error::Error for ValidationError {}

fn check_version(field: &str, version: u32, issues: &mut Vec<ValidationIssue>) {
    if version != PROTOCOL_VERSION_V1 {
        issues.push(ValidationIssue::new(
            field,
            "unsupported_version",
            format!("only version {PROTOCOL_VERSION_V1} is supported"),
        ));
    }
}

fn check_nonempty_bounded(
    field: &str,
    value: &str,
    maximum: usize,
    issues: &mut Vec<ValidationIssue>,
) {
    if value.trim().is_empty() {
        issues.push(ValidationIssue::new(
            field,
            "empty",
            "value must not be empty",
        ));
    } else if value.len() > maximum {
        issues.push(ValidationIssue::new(
            field,
            "too_long",
            format!("value must contain at most {maximum} bytes"),
        ));
    }
}

fn check_identifier(field: &str, value: &str, maximum: usize, issues: &mut Vec<ValidationIssue>) {
    check_nonempty_bounded(field, value, maximum, issues);
    if !value.is_empty()
        && !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        issues.push(ValidationIssue::new(
            field,
            "invalid_identifier",
            "identifier may contain only ASCII letters, digits, '.', '_', '-', and ':'",
        ));
    }
}

fn check_program(field: &str, value: &str, issues: &mut Vec<ValidationIssue>) {
    check_identifier(field, value, MAX_PROGRAM_LEN, issues);
}

// ---- Harness compatibility ---------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessIdentity {
    pub harness: HarnessId,
    pub harness_version: String,
    pub adapter_version: String,
}

/// Semantics an adapter has positively established for an inspected harness.
/// A version string alone is never treated as proof of compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessCapability {
    ExactModel,
    ReasoningEffort,
    WorkspaceNone,
    WorkspaceReadOnly,
    WorkspaceReadWrite,
    WorkspaceUnrestricted,
    BuiltinLocalExecution,
    BuiltinWebSearch,
    DynamicClientTools,
    RawJsonlInput,
    RawJsonlOutput,
    TurnInterruption,
    DeveloperInstructions,
    ExplicitEmptyEnvironments,
    ExperimentalRawEvents,
    PersistentFileAuthentication,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessCapabilitiesV1 {
    pub version: u32,
    pub identity: HarnessIdentity,
    pub capabilities: Vec<HarnessCapability>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsupportedSettingV1 {
    pub field: String,
    pub requested: serde_json::Value,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessCompatibilityErrorV1 {
    pub version: u32,
    pub harness: HarnessIdentity,
    pub unsupported: Vec<UnsupportedSettingV1>,
}

// ---- Lifecycle projections ---------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Accepted,
    Running,
    WaitingOnRequester,
    Completed,
    Failed,
    Cancelled,
}

impl JobState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Pending,
    Starting,
    Running,
    WaitingOnRequester,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    Lost,
}

impl AttemptState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Lost
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptTerminalReason {
    Completed,
    HarnessFailure,
    QuotaExhausted,
    ProtocolError,
    TimedOut,
    Cancelled,
    Lost,
    RequesterUnavailable,
}

/// Structured successful harness output derived from raw harness stdout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptOutputV1 {
    pub thread_id: String,
    pub turn_id: String,
    pub final_message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptV1 {
    pub version: u32,
    pub id: AttemptId,
    pub job_id: JobId,
    pub ordinal: u32,
    pub harness: HarnessIdentity,
    pub state: AttemptState,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<AttemptTerminalReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<AttemptOutputV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSummaryV1 {
    pub version: u32,
    pub id: JobId,
    pub label: String,
    pub requester: Requester,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<JobId>,
    pub state: JobState,
    pub request_digest: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_attempt_id: Option<AttemptId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobV1 {
    pub version: u32,
    pub summary: JobSummaryV1,
    pub request: JobRequestV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<AttemptV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaStatusV1>,
}

/// Compact read-time observation; runtime state never establishes requester-domain success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatusV1 {
    pub version: u32,
    pub id: JobId,
    pub requester: Requester,
    pub state: JobState,
    pub attempt_id: Option<AttemptId>,
    pub attempt_state: Option<AttemptState>,
    pub pending_tool_calls: Vec<ToolCallSummaryV1>,
    pub final_output_available: bool,
    pub terminal_reason: Option<AttemptTerminalReason>,
    pub terminal_message: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallSummaryV1 {
    pub id: ToolCallId,
    pub tool_name: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobWaitV1 {
    pub status: JobStatusV1,
    /// `terminal` or `timeout`; a timeout does not cancel the job.
    pub outcome: String,
}

/// Durable deployment admission status. Existing jobs and mailbox traffic can
/// continue while new submissions are held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceStatusV1 {
    pub protocol_version: u32,
    pub holds: Vec<String>,
    pub drained: bool,
    pub nonterminal_jobs: usize,
}

/// Identity of the deployment that owns one independent hold.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceOwnerV1 {
    pub run_id: String,
}

// ---- Schema-bound log records ------------------------------------------------

/// Exact schema document registered for one producer/protocol version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogSchemaV1 {
    pub version: u32,
    pub id: SchemaId,
    pub name: String,
    pub schema_version: String,
    pub media_type: String,
    pub producer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_version: Option<String>,
    pub schema: Box<RawValue>,
    pub digest: String,
}

impl LogSchemaV1 {
    /// Construct a schema registration and digest the exact raw schema bytes.
    #[must_use]
    pub fn new(
        id: impl Into<SchemaId>,
        name: impl Into<String>,
        schema_version: impl Into<String>,
        media_type: impl Into<String>,
        producer: impl Into<String>,
        schema: Box<RawValue>,
    ) -> Self {
        let digest = sha256_digest(schema.get().as_bytes());
        Self {
            version: PROTOCOL_VERSION_V1,
            id: id.into(),
            name: name.into(),
            schema_version: schema_version.into(),
            media_type: media_type.into(),
            producer: producer.into(),
            producer_version: None,
            schema,
            digest,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LogStream {
    #[serde(rename = "nucleus.lifecycle")]
    NucleusLifecycle,
    #[serde(rename = "nucleus.control")]
    NucleusControl,
    #[serde(rename = "harness.input")]
    HarnessInput,
    #[serde(rename = "harness.output")]
    HarnessOutput,
    #[serde(rename = "harness.stderr")]
    HarnessStderr,
    #[serde(rename = "requester")]
    Requester,
}

/// One exact harness stdout record observed during a job.
///
/// The surrounding fields are a read-time compatibility envelope. Only the
/// attempt, sequence, observation time, and exact payload bytes are persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogRecordV1 {
    pub version: u32,
    pub job_id: JobId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<AttemptId>,
    pub sequence: u64,
    pub observed_at: String,
    pub stream: LogStream,
    pub schema_id: SchemaId,
    pub payload: Box<RawValue>,
    pub payload_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEventKind {
    JobAccepted,
    JobStarted,
    AttemptCreated,
    HarnessValidated,
    ProcessStarted,
    ThreadStarted,
    TurnStarted,
    WaitingOnRequester,
    ToolCallPending,
    ToolCallAnswered,
    CancellationRequested,
    TurnCompleted,
    ProcessExited,
    AttemptCompleted,
    AttemptFailed,
    AttemptTimedOut,
    AttemptCancelled,
    AttemptLost,
    JobCompleted,
    JobFailed,
    JobCancelled,
    RecordDecodeFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleEventV1 {
    pub version: u32,
    pub event: LifecycleEventKind,
    pub job_id: JobId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<AttemptId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<RawValue>>,
}

// ---- Toolset and requester mailbox ------------------------------------------

/// One requester-owned dynamic tool. The input schema is retained verbatim and
/// separately addressable, rather than projected into Nucleus's database.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolDefinitionV1 {
    pub name: String,
    pub description: String,
    pub input_schema_id: SchemaId,
    pub input_schema: Box<RawValue>,
}

/// The stable Nucleus envelope containing a toolset's schema-bound tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolsetDefinitionsV1 {
    pub version: u32,
    pub tools: Vec<ToolDefinitionV1>,
}

impl ToolsetDefinitionsV1 {
    /// Digest the canonical v1 tool-definition envelope.
    ///
    /// # Errors
    ///
    /// Returns an error if a raw schema cannot be encoded.
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        serde_json::to_vec(self).map(|bytes| sha256_digest(&bytes))
    }
}

/// Idempotent registration of a requester-owned harness tool definition list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolsetRegistrationV1 {
    pub version: u32,
    pub toolset: ToolsetRef,
    pub definitions_schema_id: SchemaId,
    pub definitions: ToolsetDefinitionsV1,
    pub digest: String,
}

impl ToolsetRegistrationV1 {
    /// Construct an idempotent registration and derive its content digest.
    ///
    /// # Errors
    ///
    /// Returns an error if the definitions cannot be encoded.
    pub fn new(
        toolset: ToolsetRef,
        definitions_schema_id: impl Into<SchemaId>,
        definitions: ToolsetDefinitionsV1,
    ) -> Result<Self, serde_json::Error> {
        let digest = definitions.digest()?;
        Ok(Self {
            version: PROTOCOL_VERSION_V1,
            toolset,
            definitions_schema_id: definitions_schema_id.into(),
            definitions,
            digest,
        })
    }

    /// Validate versions, identities, definitions, and the supplied digest.
    ///
    /// # Errors
    ///
    /// Returns every registration violation found.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let mut issues = Vec::new();
        check_version("version", self.version, &mut issues);
        check_program("toolset.provider", &self.toolset.provider, &mut issues);
        check_identifier(
            "toolset.name",
            &self.toolset.name,
            MAX_IDENTIFIER_LEN,
            &mut issues,
        );
        if self.toolset.version == 0 {
            issues.push(ValidationIssue::new(
                "toolset.version",
                "not_positive",
                "toolset version must be greater than zero",
            ));
        }
        check_identifier(
            "definitionsSchemaId",
            self.definitions_schema_id.as_str(),
            MAX_IDENTIFIER_LEN,
            &mut issues,
        );
        check_version("definitions.version", self.definitions.version, &mut issues);
        if self.definitions.tools.is_empty() {
            issues.push(ValidationIssue::new(
                "definitions.tools",
                "empty",
                "a toolset must contain at least one tool",
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        for (index, tool) in self.definitions.tools.iter().enumerate() {
            let name_field = format!("definitions.tools[{index}].name");
            check_identifier(&name_field, &tool.name, MAX_IDENTIFIER_LEN, &mut issues);
            check_nonempty_bounded(
                &format!("definitions.tools[{index}].description"),
                &tool.description,
                16_384,
                &mut issues,
            );
            check_identifier(
                &format!("definitions.tools[{index}].inputSchemaId"),
                tool.input_schema_id.as_str(),
                MAX_IDENTIFIER_LEN,
                &mut issues,
            );
            if !names.insert(&tool.name) {
                issues.push(ValidationIssue::new(
                    name_field,
                    "duplicate",
                    "tool names must be unique within a toolset",
                ));
            }
        }
        match self.definitions.digest() {
            Ok(actual) if actual != self.digest => issues.push(ValidationIssue::new(
                "digest",
                "digest_mismatch",
                format!("expected {actual}"),
            )),
            Ok(_) => {}
            Err(error) => issues.push(ValidationIssue::new(
                "definitions",
                "not_serializable",
                error.to_string(),
            )),
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { issues })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredToolsetV1 {
    pub version: u32,
    pub toolset: ToolsetRef,
    pub definitions_schema_id: SchemaId,
    pub digest: String,
    pub registered_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallV1 {
    pub version: u32,
    pub id: ToolCallId,
    pub job_id: JobId,
    pub attempt_id: AttemptId,
    pub request_sequence: u64,
    pub tool_name: String,
    pub arguments_schema_id: SchemaId,
    pub arguments: Box<RawValue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallState {
    Pending,
    Answered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingToolCallV1 {
    pub version: u32,
    pub call: ToolCallV1,
    pub state: ToolCallState,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolResultV1 {
    pub version: u32,
    pub call_id: ToolCallId,
    pub requester: Requester,
    pub result_schema_id: SchemaId,
    pub result: Box<RawValue>,
    #[serde(default)]
    pub is_error: bool,
}

// ---- HTTP API DTOs -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobAcceptedV1 {
    pub version: u32,
    pub job_id: JobId,
    pub state: JobState,
    pub request_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<AttemptV1>,
    pub log_cursor: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListJobsQueryV1 {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester_program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<JobId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<JobState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<JobId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl ListJobsQueryV1 {
    /// Validate requester filters and the page bound.
    ///
    /// # Errors
    ///
    /// Returns an error for an incomplete requester pair or invalid limit.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let mut issues = Vec::new();
        if self.requester_id.is_some() && self.requester_program.is_none() {
            issues.push(ValidationIssue::new(
                "requesterId",
                "program_required",
                "requesterProgram is required when requesterId is supplied",
            ));
        }
        if let Some(limit) = self.limit
            && !(1..=1_000).contains(&limit)
        {
            issues.push(ValidationIssue::new(
                "limit",
                "out_of_range",
                "limit must be between 1 and 1000",
            ));
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { issues })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListJobsResponseV1 {
    pub version: u32,
    pub jobs: Vec<JobSummaryV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<JobId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogsQueryV1 {
    #[serde(default)]
    pub after: u64,
    #[serde(default)]
    pub follow: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl LogsQueryV1 {
    /// Validate the requested page bound.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is outside the supported range.
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_optional_limit(self.limit)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogsResponseV1 {
    pub version: u32,
    pub job_id: JobId,
    pub records: Vec<LogRecordV1>,
    pub next_sequence: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCallsQueryV1 {
    #[serde(default)]
    pub after: u64,
    #[serde(default)]
    pub wait_seconds: u32,
}

impl ToolCallsQueryV1 {
    /// Validate the bounded long-poll interval.
    ///
    /// # Errors
    ///
    /// Returns an error when `wait_seconds` exceeds sixty seconds.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.wait_seconds <= 60 {
            Ok(())
        } else {
            Err(ValidationError {
                issues: vec![ValidationIssue::new(
                    "waitSeconds",
                    "out_of_range",
                    "waitSeconds must be at most 60",
                )],
            })
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallsResponseV1 {
    pub version: u32,
    pub job_id: JobId,
    pub calls: Vec<PendingToolCallV1>,
    pub next_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelJobResponseV1 {
    pub version: u32,
    pub job_id: JobId,
    pub state: JobState,
    pub cancellation_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchContextAcceptedV1 {
    pub version: u32,
    pub id: LaunchContextId,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticationReadinessV1 {
    pub codex_home: AbsolutePath,
    pub configured: bool,
    pub authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Live bounded-execution capacity reported by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionCapacityV1 {
    /// Maximum number of job attempts that may own a live Codex process.
    pub max_active_jobs: u32,
    /// Slots currently held by starting, running, or requester-blocked attempts.
    pub active_jobs: u32,
    /// Slots immediately available to accepted work.
    pub available_slots: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponseV1 {
    pub version: u32,
    pub status: String,
    pub daemon_version: String,
    pub accepting_jobs: bool,
    pub checked_at: String,
    pub supported_protocol_versions: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness_executable: Option<AbsolutePath>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<HarnessCapability>,
    pub authentication: AuthenticationReadinessV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionCapacityV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaStatusV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountSnapshotQueryV1 {
    #[serde(default)]
    pub include_usage: bool,
    /// How long to wait for the canonical credential operation. Zero is a
    /// nonblocking try-lock; values are capped at thirty seconds.
    #[serde(default)]
    pub wait_seconds: u32,
}

impl AccountSnapshotQueryV1 {
    /// Validate the bounded canonical-credential wait.
    ///
    /// # Errors
    ///
    /// Returns an error when the wait exceeds thirty seconds.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.wait_seconds <= 30 {
            Ok(())
        } else {
            Err(ValidationError {
                issues: vec![ValidationIssue::new(
                    "waitSeconds",
                    "out_of_range",
                    "waitSeconds must be at most 30",
                )],
            })
        }
    }
}

/// An authenticated account read performed through Nucleus's canonical
/// credential boundary. External result objects remain opaque JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSnapshotV1 {
    pub version: u32,
    pub observed_at: String,
    pub harness: HarnessIdentity,
    pub rate_limits: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaStatusV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponseV1 {
    pub version: u32,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<ValidationIssue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

fn validate_optional_limit(limit: Option<u32>) -> Result<(), ValidationError> {
    if let Some(limit) = limit
        && !(1..=1_000).contains(&limit)
    {
        return Err(ValidationError {
            issues: vec![ValidationIssue::new(
                "limit",
                "out_of_range",
                "limit must be between 1 and 1000",
            )],
        });
    }
    Ok(())
}

/// Return a textual SHA-256 digest with an explicit algorithm prefix.
#[must_use]
pub fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(7 + digest.len() * 2);
    output.push_str("sha256:");
    for byte in digest {
        use fmt::Write as _;
        // Writing to String is infallible.
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_unknown_field(
        mut value: serde_json::Value,
        pointer: &str,
        field: &str,
    ) -> serde_json::Value {
        value
            .pointer_mut(pointer)
            .and_then(serde_json::Value::as_object_mut)
            .unwrap_or_else(|| panic!("test pointer {pointer:?} must select an object"))
            .insert(field.to_owned(), serde_json::json!("unexpected"));
        value
    }

    fn assert_unknown_field_rejected<T>(value: serde_json::Value, field: &str)
    where
        T: serde::de::DeserializeOwned,
    {
        let Err(error) = serde_json::from_value::<T>(value) else {
            panic!("unknown field {field:?} was accepted");
        };
        let message = error.to_string();
        assert!(
            message.contains("unknown field") && message.contains(field),
            "unexpected decode error for {field:?}: {message}"
        );
    }

    fn request() -> JobRequestV1 {
        let mut invocation = AgentInvocationV1::new(
            "codex-app-server",
            "gpt-5.6",
            AbsolutePath::new("/Users/example/rust/annals"),
            WorkspaceAccess::ReadOnly,
            BuiltinToolsV1 {
                local_execution: false,
                web_search: false,
            },
            TimeoutSeconds::new(3_600),
        );
        invocation.reasoning_effort = Some(ReasoningEffort::High);
        invocation.toolset = Some(ToolsetRef {
            provider: "annals".to_owned(),
            name: "liaison".to_owned(),
            version: 4,
        });

        JobRequestV1::new(
            "job_01K5Z8M9YB0FN3RQ7T4A",
            "Examine Annals work w17 at revision 204",
            Requester {
                program: "annals".to_owned(),
                id: "model-run-01K5Z8K3Q2J7".to_owned(),
            },
            "Use only the supplied Annals tools and record one reconciliation.",
            "Examine retained work w17 against corpus revision 204.",
            invocation,
        )
    }

    #[test]
    fn valid_request_round_trips_in_camel_case() {
        let request = request();
        request
            .validate()
            .unwrap_or_else(|error| panic!("fixture should validate: {error}"));

        let encoded = serde_json::to_string(&request)
            .unwrap_or_else(|error| panic!("serialize request: {error}"));
        assert!(encoded.contains("\"reasoningEffort\":\"high\""));
        assert!(encoded.contains("\"workspaceAccess\":\"read-only\""));
        assert!(
            encoded.contains("\"builtinTools\":{\"localExecution\":false,\"webSearch\":false}")
        );
        assert!(encoded.contains("\"timeoutSeconds\":3600"));
        assert!(encoded.contains("\"instructions\":"));
        assert!(!encoded.contains("reasoning_effort"));

        let decoded: JobRequestV1 = serde_json::from_str(&encoded)
            .unwrap_or_else(|error| panic!("deserialize request: {error}"));
        assert_eq!(decoded, request);
    }

    #[test]
    fn unrestricted_access_requires_the_new_invocation_version() {
        let mut request = request();
        request.invocation = AgentInvocationV1::new(
            "codex",
            "example-model",
            AbsolutePath::new("/tmp"),
            WorkspaceAccess::Unrestricted,
            BuiltinToolsV1 {
                local_execution: true,
                web_search: false,
            },
            TimeoutSeconds::new(300),
        );
        assert_eq!(request.version, PROTOCOL_VERSION_V1);
        assert_eq!(request.invocation.version, INVOCATION_VERSION_V2);
        request.validate().unwrap_or_else(|error| panic!("{error}"));
        let encoded = serde_json::to_string(&request)
            .unwrap_or_else(|error| panic!("serialize unrestricted request: {error}"));
        let decoded: JobRequestV1 = serde_json::from_str(&encoded)
            .unwrap_or_else(|error| panic!("decode unrestricted request: {error}"));
        assert_eq!(decoded, request);

        request.invocation.version = PROTOCOL_VERSION_V1;
        let error = request
            .validate()
            .err()
            .unwrap_or_else(|| panic!("version one must not admit unrestricted execution"));
        assert!(error.issues.iter().any(|issue| {
            issue.field == "invocation.workspaceAccess" && issue.code == "unsupported_setting"
        }));
    }

    #[test]
    fn checked_in_job_examples_match_the_v1_contract() {
        for encoded in [
            include_str!("../../../examples/job.example.json"),
            include_str!("../../../examples/job.todo.json"),
            include_str!("../../../examples/job.annals.json"),
        ] {
            let request: JobRequestV1 = serde_json::from_str(encoded)
                .unwrap_or_else(|error| panic!("deserialize checked-in job example: {error}"));
            request
                .validate()
                .unwrap_or_else(|error| panic!("validate checked-in job example: {error}"));
        }
    }

    #[test]
    fn checked_in_registration_examples_have_exact_digests() {
        let schema: LogSchemaV1 = serde_json::from_str(include_str!(
            "../../../examples/schema.todo-create-result.json"
        ))
        .unwrap_or_else(|error| panic!("deserialize checked-in result schema: {error}"));
        assert_eq!(schema.digest, sha256_digest(schema.schema.get().as_bytes()));

        let toolset: ToolsetRegistrationV1 =
            serde_json::from_str(include_str!("../../../examples/toolset.todo.json"))
                .unwrap_or_else(|error| panic!("deserialize checked-in toolset: {error}"));
        toolset
            .validate()
            .unwrap_or_else(|error| panic!("validate checked-in toolset: {error}"));
    }

    #[test]
    fn health_execution_capacity_is_additive_and_camel_case() {
        let legacy: HealthResponseV1 = serde_json::from_value(serde_json::json!({
            "version": 1,
            "status": "ok",
            "daemonVersion": "0.3.3",
            "acceptingJobs": true,
            "checkedAt": "2026-09-03T00:00:00Z",
            "supportedProtocolVersions": [1],
            "authentication": {
                "codexHome": "/tmp/codex-home",
                "configured": true,
                "authenticated": true
            }
        }))
        .unwrap_or_else(|error| panic!("deserialize legacy health: {error}"));
        assert_eq!(legacy.execution, None);

        let execution = ExecutionCapacityV1 {
            max_active_jobs: 8,
            active_jobs: 3,
            available_slots: 5,
        };
        let encoded = serde_json::to_value(execution)
            .unwrap_or_else(|error| panic!("serialize execution capacity: {error}"));
        assert_eq!(
            encoded,
            serde_json::json!({
                "maxActiveJobs": 8,
                "activeJobs": 3,
                "availableSlots": 5
            })
        );
    }

    #[test]
    fn request_contract_rejects_unknown_fields_at_every_configuration_level() {
        let encoded = serde_json::to_value(request())
            .unwrap_or_else(|error| panic!("serialize request: {error}"));

        for (pointer, field) in [
            ("", "priority"),
            ("/requester", "displayName"),
            ("/invocation", "providerConfig"),
            ("/invocation/builtinTools", "approvalPolicy"),
            ("/invocation/toolset", "callbackUrl"),
        ] {
            assert_unknown_field_rejected::<JobRequestV1>(
                insert_unknown_field(encoded.clone(), pointer, field),
                field,
            );
        }
    }

    #[test]
    fn validation_reports_all_stable_contract_violations() {
        let mut request = request();
        request.version = 2;
        request.parent = Some(request.id.clone());
        request.instructions = "  ".to_owned();
        request.prompt = "  ".to_owned();
        request.invocation.version = 3;
        request.invocation.cwd = AbsolutePath::new("relative/path");
        request.invocation.timeout_seconds = TimeoutSeconds::new(0);

        let error = match request.validate() {
            Ok(()) => panic!("invalid request must fail"),
            Err(error) => error,
        };
        let fields: Vec<_> = error
            .issues
            .iter()
            .map(|issue| issue.field.as_str())
            .collect();
        assert!(fields.contains(&"version"));
        assert!(fields.contains(&"parent"));
        assert!(fields.contains(&"instructions"));
        assert!(fields.contains(&"prompt"));
        assert!(fields.contains(&"invocation.version"));
        assert!(fields.contains(&"invocation.cwd"));
        assert!(fields.contains(&"invocation.timeoutSeconds"));
    }

    #[test]
    fn request_digest_is_stable_across_round_trip() {
        let request = request();
        let first = request
            .digest()
            .unwrap_or_else(|error| panic!("digest request: {error}"));
        let encoded = serde_json::to_vec(&request)
            .unwrap_or_else(|error| panic!("serialize request: {error}"));
        let decoded: JobRequestV1 = serde_json::from_slice(&encoded)
            .unwrap_or_else(|error| panic!("deserialize request: {error}"));
        let second = decoded
            .digest()
            .unwrap_or_else(|error| panic!("digest decoded request: {error}"));

        assert_eq!(first, second);
        assert_eq!(first.len(), "sha256:".len() + 64);
    }

    #[test]
    fn changing_idempotent_request_content_changes_digest() {
        let original = request();
        let mut changed = original.clone();
        changed.label.push_str(" (changed)");

        assert_ne!(
            original
                .digest()
                .unwrap_or_else(|error| panic!("digest original: {error}")),
            changed
                .digest()
                .unwrap_or_else(|error| panic!("digest changed: {error}"))
        );
    }

    #[test]
    fn raw_log_payload_is_not_restructured() {
        let raw = RawValue::from_string(
            r#"{ "method" : "turn/started", "params" : {"turnId":"t1"} }"#.to_owned(),
        )
        .unwrap_or_else(|error| panic!("valid raw JSON: {error}"));
        let original = raw.get().to_owned();
        let record = LogRecordV1 {
            version: PROTOCOL_VERSION_V1,
            job_id: JobId::from("job_1"),
            attempt_id: Some(AttemptId::from("attempt_1")),
            sequence: 1,
            observed_at: "2026-08-26T12:00:00Z".to_owned(),
            stream: LogStream::HarnessOutput,
            schema_id: SchemaId::from("codex.server-message.v1"),
            payload: raw,
            payload_digest: sha256_digest(original.as_bytes()),
        };

        let encoded = serde_json::to_string(&record)
            .unwrap_or_else(|error| panic!("serialize record: {error}"));
        let decoded: LogRecordV1 = serde_json::from_str(&encoded)
            .unwrap_or_else(|error| panic!("deserialize record: {error}"));
        assert_eq!(decoded.payload.get(), original);
        assert_eq!(decoded.payload_digest, sha256_digest(original.as_bytes()));
    }

    #[test]
    fn log_schema_constructor_digests_exact_raw_bytes() {
        let raw_schema = r#"{ "type" : "object", "required" : ["result"] }"#;
        let schema = LogSchemaV1::new(
            "todo.result.v1",
            "Todo result",
            "1",
            "application/schema+json",
            "todo",
            RawValue::from_string(raw_schema.to_owned())
                .unwrap_or_else(|error| panic!("valid schema: {error}")),
        );

        assert_eq!(schema.version, PROTOCOL_VERSION_V1);
        assert_eq!(schema.schema.get(), raw_schema);
        assert_eq!(schema.digest, sha256_digest(raw_schema.as_bytes()));
        assert_eq!(schema.producer_version, None);

        let compact_schema = r#"{"type":"object","required":["result"]}"#;
        assert_ne!(schema.digest, sha256_digest(compact_schema.as_bytes()));
    }

    #[test]
    fn unsupported_invocation_version_is_rejected() {
        let mut request = request();
        request.invocation.version = 0;
        let error = match request.validate() {
            Ok(()) => panic!("version zero must fail"),
            Err(error) => error,
        };
        assert!(error.issues.iter().any(|issue| {
            issue.field == "invocation.version" && issue.code == "unsupported_version"
        }));
    }

    #[test]
    fn toolset_registration_retains_each_raw_input_schema() {
        let input_schema = RawValue::from_string(
            r#"{ "type":"object", "required":["title"], "properties":{} }"#.to_owned(),
        )
        .unwrap_or_else(|error| panic!("valid input schema: {error}"));
        let original = input_schema.get().to_owned();
        let definitions = ToolsetDefinitionsV1 {
            version: PROTOCOL_VERSION_V1,
            tools: vec![ToolDefinitionV1 {
                name: "create_todo".to_owned(),
                description: "Create one actionable todo".to_owned(),
                input_schema_id: SchemaId::from("todo.create.input.v1"),
                input_schema,
            }],
        };
        let registration = ToolsetRegistrationV1::new(
            ToolsetRef {
                provider: "todo".to_owned(),
                name: "create".to_owned(),
                version: 1,
            },
            "nucleus.toolset-definitions.v1",
            definitions,
        )
        .unwrap_or_else(|error| panic!("construct registration: {error}"));
        registration
            .validate()
            .unwrap_or_else(|error| panic!("registration validates: {error}"));

        let encoded = serde_json::to_string(&registration)
            .unwrap_or_else(|error| panic!("serialize registration: {error}"));
        assert!(encoded.contains("\"inputSchemaId\":\"todo.create.input.v1\""));
        let decoded: ToolsetRegistrationV1 = serde_json::from_str(&encoded)
            .unwrap_or_else(|error| panic!("deserialize registration: {error}"));
        assert_eq!(decoded.definitions.tools[0].input_schema.get(), original);
        assert_eq!(decoded.digest, registration.digest);
    }

    #[test]
    fn registration_and_tool_result_contracts_reject_unknown_fields() {
        let input_schema = RawValue::from_string(r#"{"type":"object"}"#.to_owned())
            .unwrap_or_else(|error| panic!("valid input schema: {error}"));
        let definitions = ToolsetDefinitionsV1 {
            version: PROTOCOL_VERSION_V1,
            tools: vec![ToolDefinitionV1 {
                name: "create_todo".to_owned(),
                description: "Create one actionable todo".to_owned(),
                input_schema_id: SchemaId::from("todo.create.input.v1"),
                input_schema,
            }],
        };
        let registration = ToolsetRegistrationV1::new(
            ToolsetRef {
                provider: "todo".to_owned(),
                name: "create".to_owned(),
                version: PROTOCOL_VERSION_V1,
            },
            "nucleus.toolset-definitions.v1",
            definitions,
        )
        .unwrap_or_else(|error| panic!("construct registration: {error}"));
        let encoded = serde_json::to_value(registration)
            .unwrap_or_else(|error| panic!("serialize registration: {error}"));
        for (pointer, field) in [
            ("", "callbackUrl"),
            ("/toolset", "providerVersion"),
            ("/definitions", "toolOrder"),
            ("/definitions/tools/0", "approvalPolicy"),
        ] {
            assert_unknown_field_rejected::<ToolsetRegistrationV1>(
                insert_unknown_field(encoded.clone(), pointer, field),
                field,
            );
        }

        let schema = LogSchemaV1 {
            version: PROTOCOL_VERSION_V1,
            id: SchemaId::from("codex.output.v1"),
            name: "Codex output".to_owned(),
            schema_version: "1".to_owned(),
            media_type: "application/json".to_owned(),
            producer: "codex".to_owned(),
            producer_version: None,
            schema: RawValue::from_string(r#"{"type":"object"}"#.to_owned())
                .unwrap_or_else(|error| panic!("valid schema: {error}")),
            digest: "sha256:fixture".to_owned(),
        };
        assert_unknown_field_rejected::<LogSchemaV1>(
            insert_unknown_field(
                serde_json::to_value(schema)
                    .unwrap_or_else(|error| panic!("serialize schema: {error}")),
                "",
                "tableName",
            ),
            "tableName",
        );

        let result = ToolResultV1 {
            version: PROTOCOL_VERSION_V1,
            call_id: ToolCallId::from("call_1"),
            requester: Requester {
                program: "todo".to_owned(),
                id: "todo_1".to_owned(),
            },
            result_schema_id: SchemaId::from("todo.create.result.v1"),
            result: RawValue::from_string(r#"{"id":"todo_2"}"#.to_owned())
                .unwrap_or_else(|error| panic!("valid result: {error}")),
            is_error: false,
        };
        assert_unknown_field_rejected::<ToolResultV1>(
            insert_unknown_field(
                serde_json::to_value(result)
                    .unwrap_or_else(|error| panic!("serialize tool result: {error}")),
                "",
                "retryAfter",
            ),
            "retryAfter",
        );
    }

    #[test]
    fn query_contracts_reject_unknown_fields() {
        assert_unknown_field_rejected::<ListJobsQueryV1>(
            serde_json::json!({"requesterProgram": "annals", "harness": "codex"}),
            "harness",
        );
        assert_unknown_field_rejected::<LogsQueryV1>(
            serde_json::json!({"after": 4, "stream": "harness.output"}),
            "stream",
        );
        assert_unknown_field_rejected::<ToolCallsQueryV1>(
            serde_json::json!({"waitSeconds": 20, "pollInterval": 2}),
            "pollInterval",
        );
    }
}
