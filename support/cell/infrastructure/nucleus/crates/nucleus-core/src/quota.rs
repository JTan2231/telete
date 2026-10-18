//! Account-wide admission state for the main Codex weekly allowance.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaPolicyV1 {
    pub enabled: bool,
    pub pause_at_remaining_percent: u8,
    pub resume_above_remaining_percent: u8,
}

impl Default for QuotaPolicyV1 {
    fn default() -> Self {
        Self {
            enabled: true,
            pause_at_remaining_percent: 10,
            resume_above_remaining_percent: 15,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStateV1 {
    Open,
    Low,
    Exhausted,
    Unknown,
    NotApplicable,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaStatusV1 {
    pub version: u32,
    pub policy: QuotaPolicyV1,
    pub state: QuotaStateV1,
    pub account_key: Option<String>,
    pub limit_id: String,
    pub remaining_percent: Option<f64>,
    /// Time of the last successful weekly-window observation, in Unix seconds.
    pub observed_at: Option<i64>,
    pub resets_at: Option<i64>,
    /// One stable identity for a continuous admission pause, including restarts.
    pub condition_id: Option<String>,
    pub condition_started_at: Option<i64>,
}

impl QuotaStatusV1 {
    #[must_use]
    pub const fn is_blocked(&self) -> bool {
        matches!(
            self.state,
            QuotaStateV1::Low | QuotaStateV1::Exhausted | QuotaStateV1::Unknown
        )
    }

    /// Check model-work admission without treating a quota pause as a failed attempt.
    ///
    /// # Errors
    /// Returns the shared quota condition while admission is paused.
    pub fn check(&self) -> Result<(), QuotaDeferred> {
        if self.is_blocked() {
            Err(QuotaDeferred(Box::new(self.clone())))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuotaDeferred(pub Box<QuotaStatusV1>);

/// A started attempt exhausted quota. Its domain effects must be inspected before retry.
#[derive(Debug, Clone)]
pub struct QuotaExhausted;

impl std::fmt::Display for QuotaDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("model work deferred by the Codex quota gate")
    }
}
impl std::error::Error for QuotaDeferred {}
impl std::fmt::Display for QuotaExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "Codex quota exhausted during execution; inspect the retained attempt before retry",
        )
    }
}
impl std::error::Error for QuotaExhausted {}

impl crate::JobV1 {
    #[must_use]
    pub fn quota_exhausted(&self) -> bool {
        self.attempts.last().is_some_and(|attempt| {
            attempt.terminal_reason == Some(crate::AttemptTerminalReason::QuotaExhausted)
        })
    }
}

/// Classify a quota outcome without matching human-readable error text.
#[must_use]
pub fn quota_condition(mut error: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    loop {
        if error.is::<QuotaDeferred>() {
            return Some("quota_deferred");
        }
        if error.is::<QuotaExhausted>() {
            return Some("quota_exhausted");
        }
        error = error.source()?;
    }
}

/// Find a quota deferral through a requester's ordinary error context.
#[must_use]
pub fn quota_deferral<'a>(
    mut error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a QuotaDeferred> {
    loop {
        if let Some(deferred) = error.downcast_ref::<QuotaDeferred>() {
            return Some(deferred);
        }
        error = error.source()?;
    }
}

/// Select the main Codex bucket by identity and the weekly window by duration.
/// Missing, ambiguous, malformed, or expired observations remain unknown.
#[must_use]
pub fn codex_weekly_window(payload: &serde_json::Value, now: i64) -> Option<(f64, i64)> {
    let bucket = if let Some(buckets) = payload.get("rateLimitsByLimitId").filter(|v| !v.is_null())
    {
        buckets.get("codex")?
    } else {
        let bucket = payload.get("rateLimits")?;
        (bucket.get("limitId")?.as_str()? == "codex").then_some(bucket)?
    };
    if bucket
        .get("limitId")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|id| id != "codex")
    {
        return None;
    }
    let mut windows = ["primary", "secondary"]
        .into_iter()
        .filter_map(|name| bucket.get(name))
        .filter(|window| {
            window
                .get("windowDurationMins")
                .and_then(serde_json::Value::as_u64)
                == Some(10_080)
        });
    let window = windows.next()?;
    if windows.next().is_some() {
        return None;
    }
    let used = window.get("usedPercent")?.as_f64()?;
    let reset = window.get("resetsAt")?.as_i64()?;
    (used.is_finite() && (0.0..=100.0).contains(&used) && reset > now)
        .then_some((100.0 - used, reset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn weekly_selection_ignores_spark_and_rejects_unknown_data() {
        let mut payload = json!({"rateLimitsByLimitId": {
            "codex": {"limitId":"codex", "primary":{"usedPercent":92,"windowDurationMins":10080,"resetsAt":200}},
            "codex_bengalfox": {"limitId":"codex_bengalfox", "secondary":{"usedPercent":0,"windowDurationMins":10080,"resetsAt":200}}
        }});
        assert_eq!(codex_weekly_window(&payload, 100), Some((8.0, 200)));
        assert_eq!(codex_weekly_window(&payload, 200), None);
        payload["rateLimitsByLimitId"]["codex"]["secondary"] =
            payload["rateLimitsByLimitId"]["codex"]["primary"].clone();
        assert_eq!(codex_weekly_window(&payload, 100), None);
        payload["rateLimitsByLimitId"]["codex"]["primary"] = serde_json::Value::Null;
        assert_eq!(codex_weekly_window(&payload, 100), Some((8.0, 200)));
        payload["rateLimitsByLimitId"]["codex"]["secondary"]["usedPercent"] =
            serde_json::Value::Null;
        assert_eq!(codex_weekly_window(&payload, 100), None);
    }
}
