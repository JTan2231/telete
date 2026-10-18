//! Shared, product-neutral types for read-only Cell operational observations.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const STATUS_SCHEMA_VERSION: u32 = 1;
pub const REPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Active,
    OnDemand,
    Disabled,
    Retired,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionState {
    Open,
    Closed,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub state: AdmissionState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<Reason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Running,
    Idle,
    Stopped,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessState {
    Ready,
    Degraded,
    Blocked,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readiness {
    pub state: ReadinessState,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<Reason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    pub code: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Count {
    pub name: String,
    pub value: u64,
    pub unit: String,
    pub scope: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    Succeeded,
    Failed,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub kind: OutcomeKind,
    pub occurred_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub counts: Vec<Count>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_runtime_outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_domain_outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_domain_success: Option<Outcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectionReference {
    pub capability_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationalUnit {
    pub id: String,
    pub owning_product_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clockwork_key: Option<String>,
    pub intent: Intent,
    pub admission: Admission,
    pub activity: Activity,
    pub readiness: Readiness,
    #[serde(default)]
    pub evidence: Evidence,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inspection: Vec<InspectionReference>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub schema_version: u32,
    pub product_id: String,
    pub provider_release: String,
    pub observed_at_start: i64,
    pub observed_at_end: i64,
    pub complete: bool,
    pub units: Vec<OperationalUnit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduler_observations: Vec<SchedulerObservation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

/// Clockwork-owned facts that Iatreion can join to a declared scheduled unit.
/// These flags describe independent facts that can coexist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct SchedulerObservation {
    pub key: String,
    pub enabled: bool,
    pub failure_halted: bool,
    /// A retained failure episode is being checked while scheduling stays open.
    #[serde(default)]
    pub failure_pending: bool,
    pub recorded_running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incident_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_runtime_outcome: Option<Outcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    NeedsAttention,
    Operating,
    IntentionallyInactive,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeState {
    Observed,
    Undeclared,
    Unavailable,
    TimedOut,
    OutputOverflow,
    Failed,
    Invalid,
    UnsupportedSchema,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedUnit {
    pub group: Group,
    pub source_product_id: String,
    pub observation: OperationalUnit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductStatus {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub probe_state: ProbeState,
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<ReportedUnit>,
    #[serde(skip)]
    pub scheduler_observations: Vec<SchedulerObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub scope: String,
    pub root: String,
    pub observed_at_start: i64,
    pub observed_at_end: i64,
    pub products: Vec<ProductStatus>,
}

#[must_use]
pub fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

#[must_use]
pub fn static_snapshot(
    product_id: &str,
    provider_release: &str,
    units: Vec<OperationalUnit>,
) -> StatusSnapshot {
    let observed = now_unix_seconds();
    StatusSnapshot {
        schema_version: STATUS_SCHEMA_VERSION,
        product_id: product_id.to_owned(),
        provider_release: provider_release.to_owned(),
        observed_at_start: observed,
        observed_at_end: observed,
        complete: true,
        units,
        scheduler_observations: Vec::new(),
        diagnostics: Vec::new(),
    }
}

#[must_use]
pub fn on_demand_unit(product_id: &str, unit_id: &str, capability_id: &str) -> OperationalUnit {
    OperationalUnit {
        id: unit_id.to_owned(),
        owning_product_id: product_id.to_owned(),
        clockwork_key: None,
        intent: Intent::OnDemand,
        admission: Admission {
            state: AdmissionState::NotApplicable,
            reasons: Vec::new(),
        },
        activity: Activity::NotApplicable,
        readiness: Readiness {
            state: ReadinessState::Ready,
            scope: "installed local command".to_owned(),
            reasons: Vec::new(),
        },
        evidence: Evidence::default(),
        inspection: vec![InspectionReference {
            capability_id: capability_id.to_owned(),
            record_id: None,
        }],
    }
}

/// Return the standard on-demand observation when the process was invoked by
/// Iatreion as `status-snapshot --json`.
#[must_use]
pub fn requested_on_demand_snapshot_json(
    product_id: &str,
    provider_release: &str,
    unit_id: &str,
    capability_id: &str,
) -> Option<String> {
    requested_status_snapshot_json(
        product_id,
        provider_release,
        vec![on_demand_unit(product_id, unit_id, capability_id)],
        true,
    )
}

#[must_use]
pub fn status_snapshot_requested() -> bool {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    arguments.len() == 2 && arguments[0] == "status-snapshot" && arguments[1] == "--json"
}

#[must_use]
pub fn requested_status_snapshot_json(
    product_id: &str,
    provider_release: &str,
    units: Vec<OperationalUnit>,
    complete: bool,
) -> Option<String> {
    if !status_snapshot_requested() {
        return None;
    }
    let mut snapshot = static_snapshot(product_id, provider_release, units);
    snapshot.complete = complete;
    if !complete {
        snapshot.diagnostics.push(Diagnostic {
            code: "product_observation_incomplete".to_owned(),
            summary: "the product reports its declared unit but does not yet expose all local readiness evidence".to_owned(),
        });
    }
    serde_json::to_string(&snapshot).ok()
}

#[must_use]
pub fn declared_unit(
    product_id: &str,
    unit_id: &str,
    clockwork_key: Option<&str>,
    intent: Intent,
    capability_id: &str,
) -> OperationalUnit {
    let intentionally_inactive = matches!(intent, Intent::Disabled | Intent::Retired);
    OperationalUnit {
        id: unit_id.to_owned(),
        owning_product_id: product_id.to_owned(),
        clockwork_key: clockwork_key.map(str::to_owned),
        intent,
        admission: Admission {
            state: if intentionally_inactive {
                AdmissionState::NotApplicable
            } else {
                AdmissionState::Unknown
            },
            reasons: Vec::new(),
        },
        activity: if intentionally_inactive {
            Activity::NotApplicable
        } else {
            Activity::Unknown
        },
        readiness: Readiness {
            state: if intentionally_inactive {
                ReadinessState::NotApplicable
            } else {
                ReadinessState::Unknown
            },
            scope: "product-owned local prerequisites".to_owned(),
            reasons: Vec::new(),
        },
        evidence: Evidence::default(),
        inspection: vec![InspectionReference {
            capability_id: capability_id.to_owned(),
            record_id: None,
        }],
    }
}

/// Validate one product-owned observation before it enters a report.
///
/// # Errors
/// Returns a stable diagnostic string for invalid identity, time, or evidence.
pub fn validate_snapshot(snapshot: &StatusSnapshot) -> Result<(), &'static str> {
    if snapshot.schema_version != STATUS_SCHEMA_VERSION {
        return Err("unsupported schema version");
    }
    if !valid_id(&snapshot.product_id) || snapshot.provider_release.is_empty() {
        return Err("invalid product or release identity");
    }
    if snapshot.observed_at_start < 0
        || snapshot.observed_at_end < snapshot.observed_at_start
        || snapshot.observed_at_end > now_unix_seconds().saturating_add(300)
    {
        return Err("invalid observation interval");
    }
    let mut ids = BTreeSet::new();
    for unit in &snapshot.units {
        if !valid_unit_id(&unit.id)
            || !valid_id(&unit.owning_product_id)
            || !ids.insert(unit.id.as_str())
        {
            return Err("invalid or duplicate operational unit identity");
        }
        if unit.readiness.scope.trim().is_empty()
            || unit
                .admission
                .reasons
                .iter()
                .chain(&unit.readiness.reasons)
                .any(|reason| reason.code.is_empty() || reason.summary.trim().is_empty())
        {
            return Err("invalid readiness or admission evidence");
        }
    }
    Ok(())
}

#[must_use]
pub fn valid_id(value: &str) -> bool {
    value.len() <= 64
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[must_use]
pub fn valid_unit_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 129
        && value.split('/').count() == 2
        && value.split('/').all(valid_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_small_on_demand_snapshot() {
        let snapshot = static_snapshot(
            "usher",
            "0.4.0",
            vec![on_demand_unit(
                "usher",
                "usher/report",
                "usher.recognition.inspect",
            )],
        );
        assert_eq!(validate_snapshot(&snapshot), Ok(()));
        let encoded = serde_json::to_vec(&snapshot).unwrap();
        assert_eq!(
            serde_json::from_slice::<StatusSnapshot>(&encoded).unwrap(),
            snapshot
        );
    }

    #[test]
    fn keeps_multiple_admission_reasons() {
        let mut unit = on_demand_unit("annals", "annals/inbox", "annals.inbox.operate");
        unit.admission = Admission {
            state: AdmissionState::Closed,
            reasons: vec![
                Reason {
                    code: "operator_paused".into(),
                    summary: "paused by the operator".into(),
                },
                Reason {
                    code: "failure_halted".into(),
                    summary: "halted after a failure".into(),
                },
            ],
        };
        let snapshot = static_snapshot("annals", "1.0.0", vec![unit]);
        assert_eq!(snapshot.units[0].admission.reasons.len(), 2);
        assert_eq!(validate_snapshot(&snapshot), Ok(()));
    }

    #[test]
    fn reads_scheduler_observations_from_before_pending_failure_support() {
        let observation: SchedulerObservation = serde_json::from_str(
            r#"{"key":"sample/worker","enabled":true,"failure_halted":false,"recorded_running":false}"#,
        )
        .unwrap();
        assert!(!observation.failure_pending);
    }

    #[test]
    fn rejects_duplicate_units() {
        let unit = on_demand_unit("usher", "usher/report", "usher.recognition.inspect");
        let snapshot = static_snapshot("usher", "0.4.0", vec![unit.clone(), unit]);
        assert_eq!(
            validate_snapshot(&snapshot),
            Err("invalid or duplicate operational unit identity")
        );
    }
}
