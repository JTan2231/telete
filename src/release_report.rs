//! Render the same release state from the queue and from each member receipt.

use super::{Context, Job, JobId, Paths, Phase, QueueMode, Result, Store, Value, Worker, json};
use crate::store::{ReleaseFailure, ReleaseReport};

fn stopped_stage(job: &Job) -> &str {
    match job.stopped_phase.unwrap_or(job.phase) {
        Phase::Fixing => "fix",
        Phase::Analyzing => "analyze",
        Phase::Checking => "check",
        Phase::Repairing => "repair",
        Phase::Composing => "compose",
        Phase::RepairingMerge => "merge_repair",
        Phase::CheckingRelease => "release_check",
        Phase::RepairingRelease => "release_repair",
        Phase::Preparing => "prepare",
        Phase::Merging => "merge",
        Phase::Deploying => "deploy",
        phase => phase.name(),
    }
}

fn failed_run(job: &Job) -> Option<&crate::workflow::StageRun> {
    job.runs
        .last()
        .filter(|run| run.stage == stopped_stage(job) && !run.result.success())
}

pub(super) fn live_report(leader: &Job, jobs: &[Job]) -> Result<ReleaseReport> {
    let batch = leader.batch.as_ref().context("release has no membership")?;
    let failure = (leader.phase == Phase::Failed).then(|| {
        // If the leader failed while waiting for its members, report the
        // member's original failure rather than the propagated release result.
        let cause =
            if leader.stopped_phase == Some(Phase::WaitingRelease) && leader.last_error.is_none() {
                jobs.iter()
                    .find(|job| {
                        job.id != leader.id
                            && batch.members.contains(&job.id)
                            && job.phase == Phase::Failed
                            && job.stopped_phase != Some(Phase::WaitingRelease)
                    })
                    .unwrap_or(leader)
            } else {
                leader
            };
        let reason = cause.last_error.clone().or_else(|| {
            let result = &failed_run(cause)?.result;
            if result.timed_out {
                Some("command timed out".into())
            } else if let Some(signal) = result.signal {
                Some(format!("command terminated by signal {signal}"))
            } else if let Some(error) = result.start_error.as_ref().or(result.output_error.as_ref())
            {
                Some(error.clone())
            } else {
                result
                    .exit_code
                    .map(|code| format!("command exited with status {code}"))
            }
        });
        ReleaseFailure {
            job: cause.id.clone(),
            stage: stopped_stage(cause).into(),
            reason: reason
                .or_else(|| cause.outcome_message.clone())
                .unwrap_or_else(|| "release failed".into())
                .chars()
                .take(512)
                .collect(),
        }
    });
    Ok(ReleaseReport {
        id: leader.id.clone(),
        phase: leader.phase,
        members: batch.members.clone(),
        candidate_commit: leader.release_commit.clone(),
        retry_of: leader.retry_of.clone(),
        failure,
    })
}

fn job_blocker(kind: &str, job: &Job) -> Value {
    json!({"kind":kind,"job":job.id,"phase":job.phase})
}

fn scheduling_blocker(paths: &Paths, store: &Store, jobs: &[Job], leader: &Job) -> Result<Value> {
    if let Some(job) = Store::active_job(jobs)?
        && job.id != leader.id
    {
        return Ok(job_blocker(
            if job.unresolved { "unresolved" } else { "job" },
            job,
        ));
    }
    if store.paused()? {
        return Ok(json!({"kind":"paused"}));
    }
    let owners = store.holds()?;
    if !owners.is_empty() {
        return Ok(json!({"kind":"maintenance","owners":owners}));
    }
    let mode = super::queue_mode(store)?;
    if mode == QueueMode::Global
        && let Some(queue) = super::shared_queue(mode, false)?
        && let Some(holder) = queue.holder()?
        && (holder.job != leader.id || holder.root != paths.root)
    {
        if holder.root == paths.root
            && let Some(job) = jobs.iter().find(|job| job.id == holder.job)
            && !job.phase.terminal()
        {
            return Ok(job_blocker("job", job));
        }
        if holder.root != paths.root {
            return Ok(json!({"kind":"shared_slot","job":holder.job,"state_root":holder.root}));
        }
    }
    if let Some(job) = super::next_in_queue(jobs, mode)?
        && job.id != leader.id
    {
        return Ok(job_blocker("job", &job));
    }
    Ok(json!({"kind":"queue"}))
}

fn blocker(
    paths: &Paths,
    store: &Store,
    jobs: &[Job],
    leader: &Job,
) -> Result<(&'static str, Value)> {
    match leader.phase {
        Phase::WaitingRelease => {
            if let Some(member) = Worker::unready_release_member(jobs, leader)? {
                return Ok(match member.phase {
                    Phase::AwaitingRepair => ("repair", job_blocker("member", member)),
                    Phase::Blocked => ("recover", job_blocker("unresolved", member)),
                    _ => ("wait", job_blocker("member", member)),
                });
            }
            if let Some(earlier) = Worker::earlier_release_job(jobs, leader)? {
                return Ok(("wait", job_blocker("earlier_release", earlier)));
            }
            Ok(("wait", scheduling_blocker(paths, store, jobs, leader)?))
        }
        Phase::WaitingMerge if !leader.merge_requested => {
            Ok(("merge", job_blocker("manual_merge", leader)))
        }
        Phase::WaitingMerge => Ok(("wait", scheduling_blocker(paths, store, jobs, leader)?)),
        Phase::Blocked => Ok(("recover", job_blocker("unresolved", leader))),
        _ => Ok(("wait", Value::Null)),
    }
}

pub(super) fn output(
    paths: &Paths,
    store: &Store,
    jobs: &[Job],
    completions: &[Value],
    id: &JobId,
) -> Result<Value> {
    let leader = jobs.iter().find(|job| job.id == *id);
    let report = if let Some(report) = store.release_report(id)? {
        report
    } else if let Some(leader) = leader {
        live_report(leader, jobs)?
    } else {
        // Successful markers carry only their own identities. Older records
        // may also lack a stored failure report.
        let mut receipts = completions
            .iter()
            .filter(|value| value["release_id"] == id.0);
        let Some(completion) = receipts
            .clone()
            .find(|value| value["id"] == id.0)
            .or_else(|| receipts.next())
        else {
            return Ok(Value::Null);
        };
        let members = completions
            .iter()
            .rev()
            .filter(|value| value["release_id"] == id.0)
            .map(|value| serde_json::from_value(value["id"].clone()))
            .collect::<serde_json::Result<_>>()?;
        ReleaseReport {
            id: id.clone(),
            phase: serde_json::from_value(completion["phase"].clone())?,
            members,
            candidate_commit: serde_json::from_value(completion["release_commit"].clone())?,
            retry_of: if completion["id"] == id.0 {
                serde_json::from_value(completion["retry_of"].clone())?
            } else {
                None
            },
            failure: None,
        }
    };
    let (action, blocked_by) = if report.phase == Phase::Failed {
        if store.retry_available(id)? {
            ("retry", Value::Null)
        } else if let Some(leader) = leader
            && super::retry_manager::awaiting_retry_recipe(store, jobs, leader)?
        {
            ("wait", job_blocker("settling", leader))
        } else {
            ("inspect_failure", Value::Null)
        }
    } else if !report.phase.terminal()
        && let Some(leader) = leader
    {
        blocker(paths, store, jobs, leader)?
    } else {
        ("none", Value::Null)
    };
    let mut value = serde_json::to_value(&report)?;
    value["next_action"] = json!(action);
    if !blocked_by.is_null() {
        value["blocker"] = blocked_by;
    }
    if let Some(retry) = store.release_retry(id)? {
        value["retry_id"] = json!(retry);
        value["next_action"] = json!("observe_retry");
    }
    if let Some(failure) = report.failure
        && let Some(job) = jobs.iter().find(|job| job.id == failure.job)
        && stopped_stage(job) == failure.stage
    {
        // Stage finalization can reject a known command result before the
        // run is appended. Its already-written log uses the next run index.
        let next = job.next_stage_operation(&failure.stage);
        let last = job.runs.last().filter(|run| run.stage == failure.stage);
        let log = std::iter::once(next.as_str())
            .chain(last.map(|run| run.operation.as_str()))
            .map(|operation| paths.job(&job.id.0).join(format!("{operation}.log")))
            .find(|path| path.is_file());
        if let Some(log) = log {
            value["failure"]["diagnostic_path"] = json!(log);
        }
    }
    Ok(value)
}
