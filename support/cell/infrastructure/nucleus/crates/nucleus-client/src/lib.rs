//! Typed asynchronous client for a per-user `nucleusd` instance.
//!
//! The HTTP origin is intentionally synthetic: every connection is forced over
//! the configured Unix-domain socket by reqwest.

use std::env;
use std::path::{Path, PathBuf};

use nucleus_core::{
    AccountSnapshotQueryV1, AccountSnapshotV1, CancelJobResponseV1, ErrorResponseV1,
    HealthResponseV1, JobAcceptedV1, JobId, JobRequestV1, JobStatusV1, JobV1, JobWaitV1,
    LaunchContextAcceptedV1, LaunchContextRegistrationV1, ListJobsQueryV1, ListJobsResponseV1,
    LogSchemaV1, LogsQueryV1, LogsResponseV1, PendingToolCallV1, RegisteredToolsetV1, SchemaId,
    ToolCallId, ToolCallSummaryV1, ToolCallsQueryV1, ToolCallsResponseV1, ToolResultV1,
    ToolsetRegistrationV1,
};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

const ORIGIN: &str = "http://nucleus.local";

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("{0}")]
    QuotaDeferred(#[from] nucleus_core::QuotaDeferred),
    #[error("deployment readiness was not proved for the named maintenance hold")]
    DeploymentNotReady,
    #[error("HOME is unavailable or is not an absolute path; pass an explicit socket path")]
    MissingHome,
    #[error("Nucleus socket path must be absolute: {0}")]
    RelativeSocket(PathBuf),
    #[error("request validation failed: {0}")]
    Validation(#[from] nucleus_core::ValidationError),
    #[error("unable to build the local HTTP client: {0}")]
    Build(#[source] reqwest::Error),
    #[error("unable to communicate with nucleusd at {socket}: {source}")]
    Transport {
        socket: PathBuf,
        #[source]
        source: reqwest::Error,
    },
    #[error("Nucleus API returned HTTP {status}: {message} ({code})")]
    Api {
        status: u16,
        code: String,
        message: String,
        response: Box<ErrorResponseV1>,
    },
    #[error("Nucleus API returned HTTP {status} with an undecodable response: {body}")]
    UndecodableError { status: u16, body: String },
    #[error("unable to decode a successful Nucleus API response (HTTP {status}): {source}")]
    Decode {
        status: u16,
        #[source]
        source: serde_json::Error,
    },
}

/// Check runtime readiness before an installation hold or after its release.
/// A documented quota pause does not prevent installation, but this result
/// never authorizes model work or substitutes for maintenance ownership checks.
#[must_use]
pub fn unheld_deployment_ready(health: &HealthResponseV1) -> bool {
    deployment_runtime_ready(health)
        && health.status == "ok"
        && (health.accepting_jobs
            || health
                .quota
                .as_ref()
                .is_some_and(|quota| quota.version == 1 && quota.is_blocked()))
}

fn deployment_runtime_ready(health: &HealthResponseV1) -> bool {
    health.version == 1
        && health.harness.is_some()
        && health
            .harness_executable
            .as_ref()
            .is_some_and(|path| path.as_path().is_absolute())
        && health.authentication.configured
        && health.authentication.authenticated
        && health.detail.is_none()
        && health.supported_protocol_versions.contains(&1)
        && health.execution.is_some()
}

fn deployment_ready(
    health: &HealthResponseV1,
    status: &nucleus_core::MaintenanceStatusV1,
    run_id: &str,
) -> bool {
    let admission_ready = if status.holds.is_empty() {
        unheld_deployment_ready(health)
    } else {
        status.holds == [run_id]
            && status.nonterminal_jobs == 0
            && status.drained
            && health
                .execution
                .as_ref()
                .is_some_and(|capacity| capacity.active_jobs == 0)
    };
    status.protocol_version == 1 && admission_ready && deployment_runtime_ready(health)
}

/// A client whose connections are pinned to one Unix-domain socket.
#[derive(Clone, Debug)]
pub struct NucleusClient {
    socket: PathBuf,
    http: reqwest::Client,
}

impl NucleusClient {
    /// Construct a client for an explicit absolute Unix socket path.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is relative or the HTTP client cannot be
    /// constructed.
    pub fn new(socket: impl Into<PathBuf>) -> Result<Self, ClientError> {
        let socket = socket.into();
        if !socket.is_absolute() {
            return Err(ClientError::RelativeSocket(socket));
        }
        let http = reqwest::Client::builder()
            .http1_only()
            .unix_socket(socket.clone())
            .build()
            .map_err(ClientError::Build)?;
        Ok(Self { socket, http })
    }

    /// Connect using `NUCLEUS_SOCKET`, or the standard per-user path when it is
    /// not set.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket cannot be resolved or the HTTP client
    /// cannot be constructed.
    pub fn for_current_user() -> Result<Self, ClientError> {
        Self::new(default_socket_path()?)
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Read daemon health.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if the API call fails.
    pub async fn health(&self) -> Result<HealthResponseV1, ClientError> {
        self.get("/v1/health").await
    }

    /// Read health and preserve an expected quota deferral before strict readiness checks.
    ///
    /// # Errors
    /// Returns a transport error or the structured shared quota condition.
    pub async fn health_for_work(&self) -> Result<HealthResponseV1, ClientError> {
        let health = self.health().await?;
        if let Some(quota) = &health.quota {
            quota.check()?;
        }
        Ok(health)
    }

    /// Read quota admission without starting model work.
    ///
    /// # Errors
    /// Returns an error if the quota endpoint cannot be read.
    pub async fn quota_status(&self) -> Result<nucleus_core::QuotaStatusV1, ClientError> {
        self.get("/v1/quota").await
    }

    /// Prove requester deployment readiness with open admission or a quota pause,
    /// or service replacement readiness under one exact owner's sole hold.
    /// The returned health remains unchanged: `accepting_jobs` is false while
    /// held. Callers must use this result only for installation readiness, never
    /// ordinary research admission. A held service must have drained all work.
    ///
    /// # Errors
    /// Returns a [`ClientError`] unless owner, runtime drain, authentication,
    /// harness, and protocol readiness are all proved.
    pub async fn health_for_deployment(
        &self,
        run_id: &str,
    ) -> Result<HealthResponseV1, ClientError> {
        let health = self.health().await?;
        let status = self.maintenance_status().await?;
        if !deployment_ready(&health, &status, run_id) {
            return Err(ClientError::DeploymentNotReady);
        }
        Ok(health)
    }

    /// Inspect durable deployment holds and all unfinished jobs.
    ///
    /// # Errors
    /// Returns a [`ClientError`] if the API call fails.
    pub async fn maintenance_status(
        &self,
    ) -> Result<nucleus_core::MaintenanceStatusV1, ClientError> {
        self.get("/v1/maintenance").await
    }

    /// Establish one run-owned deployment admission hold.
    ///
    /// # Errors
    /// Returns a [`ClientError`] if the API call fails.
    pub async fn maintenance_hold(
        &self,
        run_id: &str,
    ) -> Result<nucleus_core::MaintenanceStatusV1, ClientError> {
        self.send_json(
            Method::POST,
            "/v1/maintenance/hold",
            &nucleus_core::MaintenanceOwnerV1 {
                run_id: run_id.into(),
            },
        )
        .await
    }

    /// Release only the named deployment's hold.
    ///
    /// # Errors
    /// Returns a [`ClientError`] if the API call fails.
    pub async fn maintenance_release(
        &self,
        run_id: &str,
    ) -> Result<nucleus_core::MaintenanceStatusV1, ClientError> {
        self.send_json(
            Method::POST,
            "/v1/maintenance/release",
            &nucleus_core::MaintenanceOwnerV1 {
                run_id: run_id.into(),
            },
        )
        .await
    }

    /// Register a short-lived, memory-only launch environment. The returned
    /// identifier may be referenced by one subsequently admitted job.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if validation or the API call fails.
    pub async fn register_launch_context(
        &self,
        context: &LaunchContextRegistrationV1,
    ) -> Result<LaunchContextAcceptedV1, ClientError> {
        context.validate()?;
        self.send_json(Method::POST, "/v1/launch-contexts", context)
            .await
    }

    /// Perform an authenticated account preflight without starting a model
    /// turn. This reads rate limits through Nucleus's canonical credential
    /// boundary.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if the account read fails.
    pub async fn account_preflight(&self) -> Result<AccountSnapshotV1, ClientError> {
        let snapshot = self
            .account_snapshot(&AccountSnapshotQueryV1 {
                include_usage: false,
                wait_seconds: 0,
            })
            .await?;
        if let Some(quota) = &snapshot.quota {
            quota.check()?;
        }
        Ok(snapshot)
    }

    /// Read account limits and, when requested, account usage through Nucleus's
    /// canonical credential boundary.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if the account read fails.
    pub async fn account_snapshot(
        &self,
        query: &AccountSnapshotQueryV1,
    ) -> Result<AccountSnapshotV1, ClientError> {
        query.validate()?;
        let response = self
            .http
            .get(Self::url("/v1/account"))
            .query(query)
            .send()
            .await
            .map_err(|source| self.transport(source))?;
        self.decode(response).await
    }

    /// Validate and submit one job.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if validation or the API call fails.
    pub async fn submit_job(&self, request: &JobRequestV1) -> Result<JobAcceptedV1, ClientError> {
        request.validate()?;
        self.send_json(Method::POST, "/v1/jobs", request).await
    }

    /// Retrieve one job and all of its attempts.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if the API call fails.
    pub async fn get_job(&self, job_id: &JobId) -> Result<JobV1, ClientError> {
        self.get(&format!("/v1/jobs/{}", path_segment(job_id.as_str())))
            .await
    }

    /// Read a requester job, yielding an accepted job while quota pauses its start.
    /// Active and terminal jobs remain observable regardless of quota admission.
    ///
    /// # Errors
    /// Returns a read error or an expected quota deferral for pending work.
    pub async fn get_job_for_work(&self, job_id: &JobId) -> Result<JobV1, ClientError> {
        let job = self.get_job(job_id).await?;
        if job.summary.state == nucleus_core::JobState::Accepted
            && let Some(quota) = &job.quota
        {
            quota.check()?;
        }
        Ok(job)
    }

    /// Observe runtime state and pending call identities without request or output bodies.
    /// Mailbox and job reads are successive observations, not an atomic snapshot.
    ///
    /// # Errors
    /// Returns a [`ClientError`] if either supported read fails.
    pub async fn job_status(&self, job_id: &JobId) -> Result<JobStatusV1, ClientError> {
        let calls = self
            .pending_tool_calls(
                job_id,
                &ToolCallsQueryV1 {
                    after: 0,
                    wait_seconds: 0,
                },
            )
            .await?;
        let job = self.get_job(job_id).await?;
        let attempt = job
            .attempts
            .iter()
            .find(|attempt| Some(&attempt.id) == job.summary.current_attempt_id.as_ref());
        let pending_tool_calls = if job.summary.state.is_terminal() {
            Vec::new()
        } else {
            calls
                .calls
                .into_iter()
                .map(|pending| ToolCallSummaryV1 {
                    id: pending.call.id,
                    tool_name: pending.call.tool_name,
                })
                .collect()
        };
        Ok(JobStatusV1 {
            version: job.version,
            id: job.summary.id,
            requester: job.summary.requester,
            state: job.summary.state,
            attempt_id: job.summary.current_attempt_id.clone(),
            attempt_state: attempt.map(|attempt| attempt.state),
            pending_tool_calls,
            final_output_available: attempt.is_some_and(|attempt| attempt.output.is_some()),
            terminal_reason: attempt.and_then(|attempt| attempt.terminal_reason),
            terminal_message: attempt.and_then(|attempt| attempt.terminal_message.clone()),
        })
    }

    /// Wait for a terminal runtime observation, returning once without cancelling on timeout.
    ///
    /// # Errors
    /// Returns a [`ClientError`] when an observation fails. The initial read must finish
    /// before a result can be returned, including with a zero timeout.
    pub async fn wait_job(
        &self,
        job_id: &JobId,
        timeout: std::time::Duration,
    ) -> Result<JobWaitV1, ClientError> {
        let started = tokio::time::Instant::now();
        let mut status = self.job_status(job_id).await?;
        loop {
            if status.state.is_terminal() {
                return Ok(JobWaitV1 {
                    status,
                    outcome: "terminal".into(),
                });
            }
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return Ok(JobWaitV1 {
                    status,
                    outcome: "timeout".into(),
                });
            };
            let observation = async {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                self.job_status(job_id).await
            };
            match tokio::time::timeout(remaining, observation).await {
                Ok(result) => status = result?,
                Err(_) => {
                    return Ok(JobWaitV1 {
                        status,
                        outcome: "timeout".into(),
                    });
                }
            }
        }
    }

    /// List jobs matching a validated query.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if validation or the API call fails.
    pub async fn list_jobs(
        &self,
        query: &ListJobsQueryV1,
    ) -> Result<ListJobsResponseV1, ClientError> {
        query.validate()?;
        let response = self
            .http
            .get(Self::url("/v1/jobs"))
            .query(query)
            .send()
            .await
            .map_err(|source| self.transport(source))?;
        self.decode(response).await
    }

    /// Fetch one bounded page of harness-output records. A daemon may hold a
    /// request with `follow=true` until at least one record becomes available or
    /// the job becomes terminal. Envelope metadata is synthesized from each
    /// output atom and its owning attempt.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if validation or the API call fails.
    pub async fn logs(
        &self,
        job_id: &JobId,
        query: &LogsQueryV1,
    ) -> Result<LogsResponseV1, ClientError> {
        query.validate()?;
        let response = self
            .http
            .get(Self::url(&format!(
                "/v1/jobs/{}/logs",
                path_segment(job_id.as_str())
            )))
            .query(query)
            .send()
            .await
            .map_err(|source| self.transport(source))?;
        self.decode(response).await
    }

    /// Request cancellation of one job.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if the API call fails.
    pub async fn cancel_job(&self, job_id: &JobId) -> Result<CancelJobResponseV1, ClientError> {
        self.send_empty(
            Method::POST,
            &format!("/v1/jobs/{}/cancel", path_segment(job_id.as_str())),
        )
        .await
    }

    /// Retrieve one exact registered schema document.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if the API call fails.
    pub async fn get_schema(&self, schema_id: &SchemaId) -> Result<LogSchemaV1, ClientError> {
        self.get(&format!("/v1/schemas/{}", path_segment(schema_id.as_str())))
            .await
    }

    /// Register an exact schema document idempotently.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if the API call fails.
    pub async fn register_schema(&self, schema: &LogSchemaV1) -> Result<LogSchemaV1, ClientError> {
        self.send_json(Method::POST, "/v1/schemas", schema).await
    }

    /// Validate and register a requester-owned toolset.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if validation or the API call fails.
    pub async fn register_toolset(
        &self,
        registration: &ToolsetRegistrationV1,
    ) -> Result<RegisteredToolsetV1, ClientError> {
        registration.validate()?;
        self.send_json(Method::POST, "/v1/toolsets", registration)
            .await
    }

    /// Retrieve a registered toolset identity and digest.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if the API call fails.
    pub async fn get_toolset(
        &self,
        provider: &str,
        name: &str,
        version: u32,
    ) -> Result<RegisteredToolsetV1, ClientError> {
        self.get(&format!(
            "/v1/toolsets/{}/{}/{version}",
            path_segment(provider),
            path_segment(name)
        ))
        .await
    }

    /// Long-poll for requester-owned managed-tool calls. `wait_seconds=0` is a
    /// non-blocking mailbox read.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if validation or the API call fails.
    pub async fn pending_tool_calls(
        &self,
        job_id: &JobId,
        query: &ToolCallsQueryV1,
    ) -> Result<ToolCallsResponseV1, ClientError> {
        query.validate()?;
        let response = self
            .http
            .get(Self::url(&format!(
                "/v1/jobs/{}/tool-calls",
                path_segment(job_id.as_str())
            )))
            .query(query)
            .send()
            .await
            .map_err(|source| self.transport(source))?;
        self.decode(response).await
    }

    /// Answer one pending managed-tool call.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] if the API call fails.
    pub async fn post_tool_result(
        &self,
        job_id: &JobId,
        call_id: &ToolCallId,
        result: &ToolResultV1,
    ) -> Result<PendingToolCallV1, ClientError> {
        self.send_json(
            Method::POST,
            &format!(
                "/v1/jobs/{}/tool-calls/{}/result",
                path_segment(job_id.as_str()),
                path_segment(call_id.as_str())
            ),
            result,
        )
        .await
    }

    async fn get<T>(&self, path: &str) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        let response = self
            .http
            .get(Self::url(path))
            .send()
            .await
            .map_err(|source| self.transport(source))?;
        self.decode(response).await
    }

    async fn send_empty<T>(&self, method: Method, path: &str) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        let response = self
            .http
            .request(method, Self::url(path))
            .send()
            .await
            .map_err(|source| self.transport(source))?;
        self.decode(response).await
    }

    async fn send_json<B, T>(&self, method: Method, path: &str, body: &B) -> Result<T, ClientError>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let response = self
            .http
            .request(method, Self::url(path))
            .json(body)
            .send()
            .await
            .map_err(|source| self.transport(source))?;
        self.decode(response).await
    }

    async fn decode<T>(&self, response: reqwest::Response) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|source| self.transport(source))?;
        if status.is_success() {
            return serde_json::from_slice(&body).map_err(|source| ClientError::Decode {
                status: status.as_u16(),
                source,
            });
        }

        let decoded = serde_json::from_slice::<ErrorResponseV1>(&body);
        if let Ok(api_error) = &decoded
            && api_error.code == "quota_deferred"
            && let Some(details) = &api_error.details
            && let Ok(quota) =
                serde_json::from_value::<nucleus_core::QuotaStatusV1>(details.clone())
            && quota.is_blocked()
        {
            return Err(nucleus_core::QuotaDeferred(Box::new(quota)).into());
        }
        match decoded {
            Ok(api_error) => Err(ClientError::Api {
                status: status.as_u16(),
                code: api_error.code.clone(),
                message: api_error.message.clone(),
                response: Box::new(api_error),
            }),
            Err(_) => Err(ClientError::UndecodableError {
                status: status.as_u16(),
                body: printable_error_body(status, &body),
            }),
        }
    }

    fn transport(&self, source: reqwest::Error) -> ClientError {
        ClientError::Transport {
            socket: self.socket.clone(),
            source,
        }
    }

    fn url(path: &str) -> String {
        debug_assert!(path.starts_with('/'));
        format!("{ORIGIN}{path}")
    }
}

/// Resolve the standard per-user socket without opening it.
///
/// # Errors
///
/// Returns an error when neither an absolute override nor an absolute home
/// directory is available.
pub fn default_socket_path() -> Result<PathBuf, ClientError> {
    if let Some(path) = env::var_os("NUCLEUS_SOCKET") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(ClientError::RelativeSocket(path));
        }
        return Ok(path);
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or(ClientError::MissingHome)?;
    Ok(home.join("Library/Application Support/Nucleus/nucleus.sock"))
}

fn printable_error_body(status: StatusCode, body: &[u8]) -> String {
    const MAX: usize = 4_096;
    if body.is_empty() {
        return status
            .canonical_reason()
            .unwrap_or("empty response body")
            .to_owned();
    }
    let prefix = &body[..body.len().min(MAX)];
    let mut text = String::from_utf8_lossy(prefix).into_owned();
    if body.len() > MAX {
        text.push('…');
    }
    text
}

fn path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            // Writing to String is infallible.
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment_health_fixture() -> Result<HealthResponseV1, serde_json::Error> {
        serde_json::from_value(serde_json::json!({
            "version":1,"status":"ok","daemonVersion":"0.5.8",
            "acceptingJobs":false,"checkedAt":"2026-09-25T00:00:00Z",
            "supportedProtocolVersions":[1],
            "harness":{"harness":"codex","harnessVersion":"0.154.0-alpha.6.2","adapterVersion":"0.5.8"},
            "harnessExecutable":"/runtime/codex",
            "authentication":{"codexHome":"/private/codex-home","configured":true,"authenticated":true},
            "execution":{"maxActiveJobs":8,"activeJobs":0,"availableSlots":8},
            "quota":{"version":1,"policy":nucleus_core::QuotaPolicyV1::default(),
                "state":"low","accountKey":"account","limitId":"codex","remainingPercent":6,
                "observedAt":100,"resetsAt":200,"conditionId":"condition","conditionStartedAt":100}
        }))
    }

    #[test]
    fn deployment_allows_documented_quota_pauses_without_allowing_work()
    -> Result<(), Box<dyn std::error::Error>> {
        use nucleus_core::QuotaStateV1;
        let mut health = deployment_health_fixture()?;
        for state in [
            QuotaStateV1::Low,
            QuotaStateV1::Exhausted,
            QuotaStateV1::Unknown,
        ] {
            health.quota.as_mut().ok_or("missing fixture quota")?.state = state;
            assert!(unheld_deployment_ready(&health));
            assert!(!health.accepting_jobs);
            assert!(
                health
                    .quota
                    .as_ref()
                    .ok_or("missing fixture quota")?
                    .check()
                    .is_err()
            );
        }
        for state in [
            QuotaStateV1::Open,
            QuotaStateV1::NotApplicable,
            QuotaStateV1::Disabled,
        ] {
            health.quota.as_mut().ok_or("missing fixture quota")?.state = state;
            assert!(!unheld_deployment_ready(&health));
        }
        health.quota = None;
        assert!(!unheld_deployment_ready(&health));
        health.accepting_jobs = true;
        assert!(unheld_deployment_ready(&health));
        Ok(())
    }

    #[test]
    fn quota_pause_cannot_hide_runtime_readiness_failures() -> Result<(), Box<dyn std::error::Error>>
    {
        use serde_json::json;
        let original = serde_json::to_value(deployment_health_fixture()?)?;
        for (pointer, invalid) in [
            ("/version", json!(2)),
            ("/status", json!("degraded")),
            ("/supportedProtocolVersions", json!([2])),
            ("/harness", json!(null)),
            ("/harnessExecutable", json!(null)),
            ("/harnessExecutable", json!("relative/codex")),
            ("/authentication/configured", json!(false)),
            ("/authentication/authenticated", json!(false)),
            ("/execution", json!(null)),
            ("/quota/version", json!(2)),
        ] {
            let mut value = original.clone();
            *value.pointer_mut(pointer).ok_or("missing fixture field")? = invalid;
            let health: HealthResponseV1 = serde_json::from_value(value)?;
            assert!(!unheld_deployment_ready(&health), "accepted {pointer}");
        }
        let mut health = deployment_health_fixture()?;
        health.detail = Some("runtime failure".into());
        assert!(!unheld_deployment_ready(&health));
        Ok(())
    }

    #[test]
    fn quota_pause_does_not_bypass_hold_ownership_or_drain()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut health = deployment_health_fixture()?;
        let mut status = nucleus_core::MaintenanceStatusV1 {
            protocol_version: 1,
            holds: vec!["owner".into()],
            drained: true,
            nonterminal_jobs: 0,
        };
        health.status = "degraded".into();
        for state in [
            nucleus_core::QuotaStateV1::Low,
            nucleus_core::QuotaStateV1::Unknown,
        ] {
            health.quota.as_mut().ok_or("missing fixture quota")?.state = state;
            assert!(deployment_ready(&health, &status, "owner"));
            assert!(!deployment_ready(&health, &status, "other"));
        }
        status.holds.push("other".into());
        assert!(!deployment_ready(&health, &status, "owner"));
        status.holds.pop();
        status.drained = false;
        assert!(!deployment_ready(&health, &status, "owner"));
        status.drained = true;
        status.nonterminal_jobs = 1;
        assert!(!deployment_ready(&health, &status, "owner"));
        status.nonterminal_jobs = 0;
        health
            .execution
            .as_mut()
            .ok_or("missing fixture capacity")?
            .active_jobs = 1;
        assert!(!deployment_ready(&health, &status, "owner"));
        Ok(())
    }

    #[test]
    fn quota_deferral_remains_classifiable_through_client_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let quota = serde_json::from_value::<nucleus_core::QuotaStatusV1>(serde_json::json!({
            "version":1, "policy":nucleus_core::QuotaPolicyV1::default(), "state":"low",
            "accountKey":"account", "limitId":"codex", "remainingPercent":9,
            "observedAt":100, "resetsAt":200, "conditionId":"condition", "conditionStartedAt":100
        }))?;
        let Err(deferred) = quota.check() else {
            return Err("low quota was admitted".into());
        };
        let error = ClientError::from(deferred);
        assert_eq!(
            nucleus_core::quota_condition(&error),
            Some("quota_deferred")
        );
        Ok(())
    }

    #[test]
    fn explicit_socket_must_be_absolute() {
        let Err(error) = NucleusClient::new("relative.sock") else {
            panic!("relative socket path must fail");
        };
        assert!(matches!(error, ClientError::RelativeSocket(_)));
    }

    #[test]
    fn printable_error_body_is_bounded() {
        let body = vec![b'x'; 5_000];
        let rendered = printable_error_body(StatusCode::BAD_REQUEST, &body);
        assert!(rendered.len() < body.len());
        assert!(rendered.ends_with('…'));
    }

    #[test]
    fn path_components_cannot_change_routes() {
        assert_eq!(path_segment("job:1"), "job%3A1");
        assert_eq!(path_segment("../health"), "..%2Fhealth");
    }
}
