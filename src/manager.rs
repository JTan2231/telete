//! Serial CI orchestration, with intent retained before effects.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cell_pipeline::{self, PreparationOutcome};
use crate::deployment::{self, DeploymentResult, DeploymentState, InstructionState, Preparation};
use crate::git;
use crate::inventory::Inventory;
use crate::invocation::CommandPolicy;
use crate::model::{CommitId, JobId};
use crate::paths::Paths;
use crate::providers::{self, Observation, Prompts, Selection};
use crate::signing::{self, SigningPolicy};
use crate::store::{
    self, Cleanup, Config, DeploymentRequest, Job, Notification, Operation, Phase, RepairAttempt,
    SendAttempt, Store, Validation,
};
use crate::validation::{self, ValidationReport, ValidationState};
use crate::workflow::{StageRun, Workflow};

#[path = "workflow_manager.rs"]
mod workflow_manager;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
// These independent caller choices are frozen together at admission.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct SubmitOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<Workflow>,
    pub request_id: Option<String>,
    pub run_tests: bool,
    pub deploy: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, Value>,
    pub repair: bool,
    pub notify: bool,
    pub no_deploy: bool,
}

impl Default for SubmitOptions {
    fn default() -> Self {
        Self {
            workflow: None,
            request_id: None,
            run_tests: false,
            deploy: Vec::new(),
            settings: BTreeMap::new(),
            repair: true,
            notify: true,
            no_deploy: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<CommandPolicy>,
    pub primary_attempts: u64,
    pub escalation_attempts: u64,
    pub primary_model: String,
    pub primary_reasoning: nucleus_core::ReasoningEffort,
    pub escalation_model: String,
    pub escalation_reasoning: nucleus_core::ReasoningEffort,
    pub model_timeout_seconds: u64,
    // Preserve the domain name in the journal alongside repair and provider policy.
    #[allow(clippy::struct_field_names)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_policy: Option<SigningPolicy>,
    pub providers: Selection,
    pub prompts: Option<Prompts>,
}

impl Policy {
    pub(crate) fn signing(&self) -> Result<&SigningPolicy> {
        self.signing_policy
            .as_ref()
            .context("Cell job has no frozen signing policy")
    }
}

fn check_identity(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "{label} must have 1 to 256 non-control characters"
    );
    Ok(())
}

fn configured(paths: &Paths) -> Result<(Store, Config)> {
    let store = Store::open(&paths.root, false)?;
    let config = store.config()?;
    paths.verify_config(&config)?;
    ensure!(
        git::common(paths, &config.repository)? == config.common_git_dir,
        "configured Git repository identity changed"
    );
    Ok((store, config))
}

fn freeze_policy(paths: &Paths, options: &SubmitOptions) -> Result<Policy> {
    let signing_policy = if options.workflow.is_none() {
        let selected = signing::selected(paths)?;
        signing::preflight(&selected)?;
        Some(selected)
    } else {
        None
    };
    let (providers, prompts) = providers::select(options.repair, options.notify)?;
    Ok(Policy {
        commands: Some(CommandPolicy::capture(paths)?),
        primary_attempts: 3,
        escalation_attempts: 1,
        primary_model: "gpt-5.6-terra".into(),
        primary_reasoning: nucleus_core::ReasoningEffort::Medium,
        escalation_model: "gpt-5.6-sol".into(),
        escalation_reasoning: nucleus_core::ReasoningEffort::High,
        model_timeout_seconds: 600,
        signing_policy,
        providers,
        prompts,
    })
}

pub(crate) fn init(paths: &Paths, repo: &Path, accepted_baseline: Option<&str>) -> Result<Value> {
    initialize(paths, repo, accepted_baseline, accepted_baseline.is_none())
}

pub(crate) fn init_project(
    paths: &Paths,
    repo: &Path,
    accepted_baseline: Option<&str>,
) -> Result<Value> {
    initialize(paths, repo, accepted_baseline, true)
}

fn initialize(
    paths: &Paths,
    repo: &Path,
    accepted_baseline: Option<&str>,
    project: bool,
) -> Result<Value> {
    ensure!(
        project || paths.local_workspace().is_none(),
        "Cell queues cannot select a local workspace"
    );
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let store = Store::open(&paths.root, true)?;
    let repository = git::repository(paths, repo)?;
    let baseline = accepted_baseline
        .map(|value| git::commit(paths, &repository, value))
        .transpose()?;
    let config = Config {
        project,
        workspace: paths.local_workspace(),
        common_git_dir: git::common(paths, &repository)?,
        repository,
        accepted_baseline: baseline.clone(),
    };
    if let Some(previous) = store.get::<Config>("config")? {
        ensure!(
            previous == config,
            "initialization cannot replace Telete's repository, baseline, or workspace"
        );
        if let Some(baseline) = &baseline {
            ensure!(
                git::ref_value(paths, &config.repository, config.accepted_ref())?.as_ref()
                    == Some(baseline),
                "accepted history already advanced; initialization cannot rewind it"
            );
        }
        return Ok(
            json!({"state":"initialized","paused":store.paused()?,"accepted":baseline,"config":config}),
        );
    }
    if let Some(baseline) = &baseline {
        git::advance(
            paths,
            &config.repository,
            config.accepted_ref(),
            None,
            baseline,
        )?;
    }
    store.transaction(|| {
        store.set("config", &config)?;
        store.set("paused", &true)
    })?;
    Ok(json!({"state":"initialized","paused":true,"accepted":baseline,"config":config}))
}

pub(crate) fn submit(
    paths: &Paths,
    repo: &Path,
    revision: &str,
    mut options: SubmitOptions,
) -> Result<Value> {
    let (store, config) = configured(paths)?;
    ensure!(
        git::common(paths, repo)? == config.common_git_dir,
        "submit from a worktree of Telete's configured repository"
    );
    let input = git::commit(paths, repo, revision)?;
    if let Some(workflow) = &options.workflow {
        ensure!(
            config.project,
            "project workflows require a separate project queue; Cell queues retain Cell validation and signing"
        );
        workflow.validate()?;
        ensure!(
            !options.run_tests
                && options.deploy.is_empty()
                && options.settings.is_empty()
                && !options.no_deploy,
            "workflow jobs select their stages instead of Cell options"
        );
        options.repair &= workflow.repair;
        options.notify &= workflow.notify;
        if workflow.integrate {
            ensure!(
                config.accepted_baseline.is_some(),
                "integration requires an initialized accepted baseline"
            );
        }
    } else {
        ensure!(!config.project, "project queues require --workflow");
        ensure!(
            config.accepted_baseline.is_some(),
            "Cell delivery requires an initialized accepted baseline"
        );
    }
    options.deploy.sort();
    options.deploy.dedup();
    for product in &options.deploy {
        crate::model::ProductId::new(product)?;
    }
    for (product, settings) in &options.settings {
        crate::model::ProductId::new(product)?;
        ensure!(
            product != "decisions",
            "use canonical krisis settings for decisions"
        );
        ensure!(
            settings.is_object(),
            "product settings must be JSON objects"
        );
        ensure!(
            options.deploy.iter().any(|selected| {
                selected == product || (selected == "decisions" && product == "krisis")
            }),
            "settings require an explicit --deploy selection for {product}"
        );
    }
    ensure!(
        !options.deploy.iter().any(|product| product == "telete"),
        "Telete installation is separate; use telete install explicitly"
    );
    ensure!(
        !options.no_deploy || options.deploy.is_empty(),
        "--no-deploy conflicts with explicit deployment products"
    );
    let request_id = options
        .request_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    check_identity(&request_id, "submission request identity")?;
    options.request_id = Some(request_id.clone());
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    if let Some(previous) = store.by_request(&request_id)? {
        ensure!(
            previous.input == input && previous.options == options,
            "submission request identity belongs to different inputs"
        );
        return previous.output();
    }
    paths.require_capacity()?;
    let policy = freeze_policy(paths, &options)?;
    let id = JobId::new(uuid::Uuid::now_v7().to_string())?;
    let job = new_job(id, request_id, input, options, policy);
    git::advance(
        paths,
        repo,
        &git::private_ref(&job.id.0, "input")?,
        None,
        &job.input,
    )?;
    let admitted = store.transaction(|| store.insert(&job))?;
    admitted.output()
}

pub(crate) fn read_settings(path: Option<&Path>) -> Result<BTreeMap<String, Value>> {
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    ensure!(path.is_absolute(), "settings path must be absolute");
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "settings must be a regular JSON file"
    );
    serde_json::from_slice(&fs::read(path)?)
        .context("settings must map product IDs to JSON objects")
}

fn new_job(
    id: JobId,
    request_id: String,
    input: CommitId,
    options: SubmitOptions,
    policy: Policy,
) -> Job {
    Job {
        id,
        sequence: 0,
        revision: 0,
        request_id,
        phase: Phase::Queued,
        cancel_requested: false,
        created: store::now(),
        updated: store::now(),
        input,
        options,
        policy,
        base: None,
        candidate: None,
        accepted: false,
        attempts: Vec::new(),
        workflow_runs: Vec::new(),
        validations: Vec::new(),
        preparation: None,
        preparation_generation: 0,
        production_diagnostics: None,
        deployment_request: None,
        deployment_result: None,
        operation: None,
        outcome: None,
        outcome_message: None,
        stopped_phase: None,
        unresolved: false,
        model_unresolved: false,
        waiting_reason: None,
        last_error: None,
        notification: None,
        outcome_generation: 1,
        cleanup: Cleanup::default(),
    }
}

pub(crate) fn status(paths: &Paths, id: Option<&str>) -> Result<Value> {
    let (store, config) = configured(paths)?;
    if let Some(id) = id {
        return store.job(id)?.output();
    }
    let active = store.active()?.map(|job| job.output()).transpose()?;
    let queue = store
        .jobs()?
        .into_iter()
        .filter(|job| job.phase == Phase::Queued)
        .map(|job| job.output())
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"schema_version":1,"version":env!("CARGO_PKG_VERSION"),"paused":store.paused()?,
        "config":config,"accepted":git::ref_value(paths,&config.repository,config.accepted_ref())?,"active":active,"queue":queue,"maintenance_owners":store.holds()?}),
    )
}

pub(crate) async fn wait(paths: &Paths, id: &str, timeout_seconds: u64) -> Result<Value> {
    ensure!(timeout_seconds > 0, "wait timeout must be positive");
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        let (store, _) = configured(paths)?;
        let job = store.job(id)?;
        if job.phase.terminal() || job.phase == Phase::Blocked {
            return Ok(json!({"observation":"terminal","job":job.output()?}));
        }
        if Instant::now() >= deadline {
            return Ok(json!({"observation":"timeout","job":job.output()?}));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub(crate) fn pause(paths: &Paths) -> Result<Value> {
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, _) = configured(paths)?;
    store.set("paused", &true)?;
    status(paths, None)
}

pub(crate) fn resume(paths: &Paths) -> Result<Value> {
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, _) = configured(paths)?;
    if let Some(active) = store.active()? {
        ensure!(
            active.phase != Phase::Blocked && !active.unresolved,
            "recover the unresolved Telete job before resuming"
        );
    }
    store.set("paused", &false)?;
    status(paths, None)
}

pub(crate) fn cancel(paths: &Paths, id: &str) -> Result<Value> {
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, config) = configured(paths)?;
    let mut job = store.job(id)?;
    if job.phase == Phase::Blocked
        && job.stopped_phase == Some(Phase::Checking)
        && job.options.workflow.is_none()
        && job
            .notification
            .as_ref()
            .is_some_and(|notice| notice.accepted && !notice.uncertain)
        && let (Some(operation), Some(base), Some(candidate)) =
            (&job.operation, &job.base, &job.candidate)
        && !operation.result.try_exists()?
        && !validation::receipt_path(paths, base, candidate, job.options.run_tests, true)
            .try_exists()?
    {
        abandon_settled_validation(paths, &store, &config, &mut job)?;
        return store.job(id)?.output();
    }
    store.transaction(|| {
        let mut job = store.job(id)?;
        store.cancel(id)?;
        if job.phase == Phase::Queued {
            job.cancel_requested = true;
            job.phase = Phase::Cancelled;
            job.outcome = Some(Phase::Cancelled);
            job.outcome_message = Some("Cancelled before queue admission".into());
            store.save(&mut job)?;
        }
        Ok(())
    })?;
    store.job(id)?.output()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidationAbandonment {
    schema: u32,
    job: JobId,
    base: CommitId,
    operation: Operation,
    abandoned: u64,
}

// Cancellation observes terminal children and retains intent; it never runs validation.
#[allow(clippy::too_many_lines)]
fn abandon_settled_validation(
    paths: &Paths,
    store: &Store,
    config: &Config,
    job: &mut Job,
) -> Result<()> {
    ensure!(
        store.paused()?
            && !config.project
            && job.options.workflow.is_none()
            && job.phase == Phase::Blocked
            && job.stopped_phase == Some(Phase::Checking)
            && job.outcome == Some(Phase::Failed)
            && job.unresolved
            && !job.model_unresolved
            && !job.accepted
            && job.preparation.is_none()
            && job.preparation_generation == 0
            && job.production_diagnostics.is_none()
            && job.deployment_request.is_none()
            && job.deployment_result.is_none()
            && job.options.notify
            && job
                .notification
                .as_ref()
                .is_some_and(|notice| notice.accepted && !notice.uncertain)
            && store.jobs()?.iter().all(|other| {
                other.id == job.id
                    || (other.phase.terminal() && !other.unresolved && !other.model_unresolved)
            }),
        "validation abandonment requires one paused unaccepted blocked validation with a settled failure notice"
    );
    let operation = job
        .operation
        .as_ref()
        .context("blocked validation has no retained operation")?;
    let base = job.base.as_ref().context("fixed base absent")?;
    let candidate = job.candidate.as_ref().context("candidate absent")?;
    let directory = paths.job(&job.id.0);
    let name = format!("validation-{}", job.validations.len());
    ensure!(
        operation.phase == Phase::Checking
            && operation.name == name
            && operation.candidate == *candidate
            && operation.result == directory.join(format!("{name}.json"))
            && !operation.result.try_exists()?
            && !validation::receipt_path(paths, base, candidate, job.options.run_tests, true)
                .try_exists()?,
        "validation has a different intent or a terminal report; use its recovery path"
    );
    let _resources = crate::broker::settled_guards(paths)?;
    let _deployment = crate::paths::lock(&paths.root.join("deployments/deployment.lock"), false)?;
    ensure!(
        !paths.root.join("deployments/active.json").try_exists()?
            && git::ref_value(paths, &config.repository, git::ACCEPTED)?.as_ref() == Some(base)
            && git::ref_value(
                paths,
                &config.repository,
                &git::private_ref(&job.id.0, "candidate")?,
            )?
            .as_ref()
                == Some(candidate),
        "accepted source, private candidate or deployment ownership changed"
    );
    let worktree = directory.join("worktree");
    ensure!(
        git::common(paths, &worktree)? == config.common_git_dir,
        "validation worktree belongs to another repository"
    );
    git::clean_candidate(paths, &worktree, candidate)?;
    if validation::require_timed_out_dispatcher(
        paths,
        &worktree,
        base,
        candidate,
        job.options.run_tests,
        true,
    )
    .is_err()
    {
        validation::require_failed_bootstrap(paths, &worktree, job)
            .context("validation abandonment requires an exact timed-out dispatcher or failed bootstrap without dispatcher admission")?;
    }
    let record = directory.join(format!("{name}.abandoned.json"));
    if record.try_exists()? {
        let previous: ValidationAbandonment = serde_json::from_slice(&fs::read(&record)?)?;
        ensure!(
            previous.schema == 1
                && previous.job == job.id
                && previous.base == *base
                && serde_json::to_value(previous.operation)? == serde_json::to_value(operation)?,
            "retained validation abandonment names another operation"
        );
    } else {
        store::atomic_json(
            &record,
            &ValidationAbandonment {
                schema: 1,
                job: job.id.clone(),
                base: base.clone(),
                operation: operation.clone(),
                abandoned: store::now(),
            },
        )?;
    }
    store.transaction(|| {
        store.cancel(&job.id.0)?;
        job.cancel_requested = true;
        job.operation = None;
        job.unresolved = false;
        job.phase = Phase::Cancelled;
        store.save(job)
    })
}

pub(crate) fn maintenance_hold(paths: &Paths, owner: &str) -> Result<Value> {
    maintenance(paths, Some(owner), "hold")
}
pub(crate) fn maintenance_status(paths: &Paths) -> Result<Value> {
    maintenance(paths, None, "status")
}
pub(crate) fn maintenance_release(paths: &Paths, owner: &str) -> Result<Value> {
    maintenance(paths, Some(owner), "release")
}

fn maintenance(paths: &Paths, owner: Option<&str>, action: &str) -> Result<Value> {
    if let Some(owner) = owner {
        check_identity(owner, "maintenance owner")?;
    }
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, _) = configured(paths)?;
    if let Some(owner) = owner {
        if action == "hold" {
            store.hold(owner)?;
        } else if action == "release" {
            store.release(owner)?;
        }
    }
    let active = store.active()?;
    let owners = store.holds()?;
    let drained = active.is_none()
        && store
            .jobs()?
            .iter()
            .all(|job| !job.unresolved && !job.model_unresolved);
    Ok(json!({"held":!owners.is_empty(),"drained":drained,
        "owner_held":owner.map(|owner| owners.iter().any(|held| held == owner)),"owners":owners,"active_job":active.map(|job|job.id)}))
}

pub(crate) fn require_quiescent(paths: &Paths) -> Result<()> {
    let database = paths.root.join("queue.sqlite3");
    if !database.exists() {
        return Ok(());
    }
    let store = Store::open(&paths.root, false)?;
    ensure!(
        store.paused()?,
        "pause Telete before installation or signing maintenance"
    );
    ensure!(
        store
            .jobs()?
            .iter()
            .all(|job| job.phase.terminal() && !job.unresolved && !job.model_unresolved),
        "Telete has queued, active or unresolved work; settle it before maintenance"
    );
    Ok(())
}

// Service stop holds broker and deployment ownership for these recovery cases.
// Installation and signing retain the stricter quiescent queue requirement.
pub(crate) fn require_service_stoppable(paths: &Paths) -> Result<()> {
    if require_quiescent(paths).is_ok() {
        return Ok(());
    }
    let (store, config) = configured(paths)?;
    ensure!(store.paused()?, "pause Telete before stopping its service");
    let job = store
        .active()?
        .context("Telete has unsettled queued work")?;
    if job.stopped_phase == Some(Phase::Checking) {
        return require_blocked_autofix_stoppable(paths, &store, &config, &job);
    }
    if job.stopped_phase == Some(Phase::Preparing) {
        return require_blocked_preparation_stoppable(paths, &store, &config, &job);
    }
    ensure!(
        job.phase == Phase::Blocked
            && job.stopped_phase == Some(Phase::Deploying)
            && job.accepted
            && job.operation.is_none()
            && !job.model_unresolved
            && (!job.options.notify
                || job
                    .notification
                    .as_ref()
                    .is_some_and(|notice| notice.accepted && !notice.uncertain))
            && store.jobs()?.iter().all(|other| {
                other.id == job.id
                    || (other.phase.terminal() && !other.unresolved && !other.model_unresolved)
            }),
        "service stop requires settled work or one blocked completed deployment"
    );
    let request = job
        .deployment_request
        .as_ref()
        .context("blocked deployment has no saved request")?;
    ensure!(
        job.candidate.as_ref() == Some(&request.source)
            && git::ref_value(paths, &config.repository, git::ACCEPTED)?.as_ref()
                == Some(&request.source)
            && !paths.root.join("deployments/active.json").try_exists()?,
        "blocked deployment source or execution ownership is unsettled"
    );
    let result = deployment::observe(paths, &request.id)?
        .context("blocked deployment has no terminal receipt")?;
    correlate_deployment(&result, request)?;
    ensure!(
        result.state == DeploymentState::Succeeded,
        "service stop requires the blocked deployment to have completed successfully"
    );
    Ok(())
}

// Stop only; acknowledgement still owns abandonment after worker shutdown.
fn require_blocked_preparation_stoppable(
    paths: &Paths,
    store: &Store,
    config: &Config,
    job: &Job,
) -> Result<()> {
    ensure!(
        !config.project
            && job.options.workflow.is_none()
            && job.phase == Phase::Blocked
            && job.stopped_phase == Some(Phase::Preparing)
            && job.outcome == Some(Phase::Failed)
            && job.unresolved
            && !job.model_unresolved
            && !job.accepted
            && job.preparation.is_none()
            && job.deployment_request.is_none()
            && job.deployment_result.is_none()
            && (!job.options.notify
                || job
                    .notification
                    .as_ref()
                    .is_some_and(|notice| notice.accepted && !notice.uncertain))
            && store.jobs()?.iter().all(|other| {
                other.id == job.id
                    || (other.phase.terminal() && !other.unresolved && !other.model_unresolved)
            }),
        "service stop requires one unaccepted blocked preparation with a settled failure notice"
    );
    let operation = job
        .operation
        .as_ref()
        .context("blocked preparation has no retained operation")?;
    let base = job.base.as_ref().context("fixed base absent")?;
    let candidate = job.candidate.as_ref().context("candidate absent")?;
    let directory = paths.job(&job.id.0);
    let name = format!("production-{}", job.preparation_generation);
    ensure!(
        operation.phase == Phase::Preparing
            && operation.name == name
            && operation.candidate == *candidate
            && operation.result == directory.join(format!("{name}.json"))
            && fs::symlink_metadata(paths.root.join("deployments/active.json"))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            && git::ref_value(paths, &config.repository, git::ACCEPTED)?.as_ref() == Some(base)
            && git::ref_value(
                paths,
                &config.repository,
                &git::private_ref(&job.id.0, "candidate")?,
            )?
            .as_ref()
                == Some(candidate),
        "blocked preparation intent, source or deployment ownership changed"
    );
    for receipt in [
        operation.result.clone(),
        directory.join(&name).join("result.json"),
    ] {
        ensure!(
            fs::symlink_metadata(receipt)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "preparation has a terminal receipt or inaccessible path; use its recovery path"
        );
    }
    let worktree = directory.join("worktree");
    ensure!(
        git::common(paths, &worktree)? == config.common_git_dir,
        "preparation worktree belongs to another repository"
    );
    git::clean_candidate(paths, &worktree, candidate)?;
    if job.policy.commands.is_some() {
        // The outer preparation supervisor is independent of broker gates.
        let command = cell_pipeline::preparation_command(paths, job)?;
        let invocation = crate::invocation::directory(paths, job, &name)?;
        let saved: crate::process::CommandSpec =
            serde_json::from_slice(&fs::read(invocation.join("request.json"))?)?;
        ensure!(
            serde_json::to_value(saved)? == serde_json::to_value(command)?,
            "preparation invocation does not match its job"
        );
        let result = crate::process::observe(&invocation)?
            .context("preparation invocation has no terminal completion")?;
        let code = result
            .exit_code
            .context("preparation invocation has no normal exit status")?;
        ensure!(
            code > 0 && result.exited_with(&[code]),
            "preparation invocation has no ordinary failed completion"
        );
    }
    Ok(())
}

// A completed autofix can be recovered after stop; this guard changes no journal state.
#[allow(clippy::too_many_lines)]
fn require_blocked_autofix_stoppable(
    paths: &Paths,
    store: &Store,
    config: &Config,
    job: &Job,
) -> Result<()> {
    ensure!(
        job.phase == Phase::Blocked
            && job.stopped_phase == Some(Phase::Checking)
            && job.outcome == Some(Phase::Failed)
            && job.unresolved
            && !job.model_unresolved
            && !job.accepted
            && job.preparation.is_none()
            && job.preparation_generation == 0
            && job.production_diagnostics.is_none()
            && job.deployment_request.is_none()
            && job.deployment_result.is_none()
            && job.options.notify
            && job
                .notification
                .as_ref()
                .is_some_and(|notice| notice.accepted && !notice.uncertain)
            && store.jobs()?.iter().all(|other| {
                other.id == job.id
                    || (other.phase.terminal() && !other.unresolved && !other.model_unresolved)
            }),
        "service stop requires one unaccepted blocked autofix with a settled failure notice"
    );
    let operation = job
        .operation
        .as_ref()
        .context("blocked autofix has no retained operation")?;
    let base = job.base.as_ref().context("fixed base absent")?;
    let candidate = job.candidate.as_ref().context("candidate absent")?;
    let directory = paths.job(&job.id.0);
    let name = format!("validation-{}", job.validations.len());
    ensure!(
        operation.phase == Phase::Checking
            && operation.name == name
            && operation.candidate == *candidate
            && operation.result == directory.join(format!("{name}.json"))
            && !paths.root.join("deployments/active.json").try_exists()?
            && git::ref_value(paths, &config.repository, git::ACCEPTED)?.as_ref() == Some(base)
            && git::ref_value(
                paths,
                &config.repository,
                &git::private_ref(&job.id.0, "candidate")?,
            )?
            .as_ref()
                == Some(candidate),
        "blocked autofix intent, source or deployment ownership changed"
    );
    let canonical = validation::receipt_path(paths, base, candidate, job.options.run_tests, true);
    let bytes = fs::read(&operation.result)?;
    let report: ValidationReport = serde_json::from_slice(&bytes)?;
    ensure!(
        report.state == ValidationState::Autofix
            && serde_json::from_slice::<Value>(&bytes)?
                == serde_json::from_slice::<Value>(&fs::read(&canonical)?)?,
        "blocked autofix has no matching terminal aggregate reports"
    );
    validation::verify_report(&report, base, candidate, job.options.run_tests, true)?;
    let patch = report
        .autofix_patch
        .as_ref()
        .context("autofix receipt has no patch")?;
    ensure!(
        *patch
            == canonical
                .parent()
                .context("validation receipt directory absent")?
                .join("autofix.patch")
            && !fs::read_to_string(patch)?.is_empty(),
        "blocked autofix has no exact retained patch"
    );
    let worktree = directory.join("worktree");
    ensure!(
        git::common(paths, &worktree)? == config.common_git_dir,
        "validation worktree belongs to another repository"
    );
    git::clean_candidate(paths, &worktree, candidate)?;
    let names = Inventory::load(&worktree)?
        .select(&report.products)?
        .iter()
        .map(|product| product.id.to_string())
        .collect::<BTreeSet<_>>();
    ensure!(
        names.len() == report.products.len() && names == report.products.iter().cloned().collect(),
        "validation selection is not a canonical product scope"
    );
    validation::require_job_dispatcher(paths, &worktree, job)
}

// Keep the authoritative recovery decisions together before the state transition.
#[allow(clippy::too_many_lines)]
pub(crate) async fn recover(paths: &Paths, id: &str) -> Result<Value> {
    let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
    let (store, config) = configured(paths)?;
    let mut job = store.job(id)?;
    ensure!(
        job.phase == Phase::Blocked,
        "only a blocked Telete job can be recovered"
    );
    if job
        .notification
        .as_ref()
        .is_some_and(|notice| notice.uncertain)
        && !job.unresolved
    {
        bail!("Email acceptance remains uncertain; recovery cannot send a replacement message");
    }
    let phase = job
        .stopped_phase
        .context("blocked job has no retained operation phase")?;
    let settled_cancel = phase == Phase::Checking
        && job.cancel_requested
        && job.operation.is_none()
        && job
            .validations
            .last()
            .is_some_and(|validation| validation.report.state == ValidationState::Stale);
    match phase {
        Phase::Checking if settled_cancel => {}
        Phase::Checking | Phase::Preparing | Phase::Deploying if job.options.workflow.is_some() => {
            let inspector = Worker {
                paths,
                store: Store::open(&paths.root, false)?,
                config: config.clone(),
            };
            inspector.reconcile_workflow(&job)?;
        }
        Phase::RepairWait => {
            let attempt = job
                .attempts
                .last_mut()
                .context("blocked repair has no saved request")?;
            let client = providers::client(&job.policy.providers)?;
            match providers::get_job(&client, &attempt.request.id).await {
                Ok(view) => {
                    if matches!(
                        providers::correlate(&view, &attempt.request)?,
                        Observation::Failed { lost: true, .. }
                    ) {
                        bail!(
                            "Nucleus still reports lost execution; the request cannot be replaced"
                        );
                    }
                    attempt.transport_failures = 0;
                    job.model_unresolved = true;
                }
                Err(error) if providers::absent(&error) => {
                    attempt.transport_failures = 0;
                    job.model_unresolved = false;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Phase::Checking | Phase::Preparing => {
            let operation = job
                .operation
                .as_ref()
                .context("blocked operation has no retained intent")?;
            if phase == Phase::Checking {
                validation::require_job_dispatcher(
                    paths,
                    &paths.job(&job.id.0).join("worktree"),
                    &job,
                )?;
            }
            if phase == Phase::Checking && !operation.result.is_file() {
                let provider_receipt = validation::receipt_path(
                    paths,
                    job.base.as_ref().context("fixed base absent")?,
                    job.candidate.as_ref().context("candidate absent")?,
                    job.options.run_tests,
                    true,
                );
                if provider_receipt.is_file() {
                    let report: ValidationReport =
                        serde_json::from_slice(&fs::read(provider_receipt)?)?;
                    store::atomic_json(&operation.result, &report)?;
                }
            }
            if phase == Phase::Preparing && !operation.result.is_file() {
                let output = paths
                    .job(&job.id.0)
                    .join(format!("production-{}", job.preparation_generation))
                    .join("result.json");
                if output.is_file() {
                    let prepared: Preparation = serde_json::from_slice(&fs::read(output)?)?;
                    let inspector = Worker {
                        paths,
                        store: Store::open(&paths.root, false)?,
                        config: config.clone(),
                    };
                    let products = inspector.production_products(&job)?;
                    correlate_preparation(
                        &prepared,
                        job.candidate.as_ref().context("candidate absent")?,
                        &products,
                        job.policy.signing()?,
                        paths,
                    )?;
                    store::atomic_json(
                        &operation.result,
                        &PreparationOutcome {
                            prepared: Some(prepared),
                            compilation_failure: None,
                        },
                    )?;
                }
            }
            ensure!(
                operation.result.is_file(),
                "operation has no authoritative terminal receipt; it cannot be repeated"
            );
        }
        Phase::Deploying => {
            let request = job
                .deployment_request
                .as_ref()
                .context("blocked deployment has no saved request")?;
            let observed = deployment::observe(paths, &request.id)?
                .context("deployment admission remains unknown")?;
            correlate_deployment(&observed, request)?;
            ensure!(
                observed.state != DeploymentState::Running
                    && (observed.state != DeploymentState::Interrupted || observed.acknowledged),
                "deployment has interrupted or uncertain instruction effects; inspect its owning operation"
            );
        }
        Phase::Accepting | Phase::Applying | Phase::Autofixing | Phase::Integrating => {}
        _ => bail!("recovery has no authoritative continuation for this phase"),
    }
    job.phase = phase;
    job.outcome_generation += 1;
    job.outcome = None;
    job.outcome_message = None;
    job.notification = None;
    job.unresolved = false;
    job.last_error = None;
    job.waiting_reason = None;
    store.save(&mut job)?;
    let mut worker = Worker {
        paths,
        store,
        config,
    };
    if settled_cancel {
        worker.finish(
            &mut job,
            Phase::Cancelled,
            "Cancelled after stale validation settled".into(),
            false,
        )?;
    } else {
        worker.tick().await?;
    }
    worker.store.job(id)?.output()
}

pub(crate) async fn worker_once(paths: &Paths) -> Result<Value> {
    let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
    let (store, config) = configured(paths)?;
    let mut worker = Worker {
        paths,
        store,
        config,
    };
    worker.tick().await
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparationAcknowledgement {
    schema: u32,
    job: JobId,
    operation: Operation,
    acknowledged: u64,
}

// Keep the identity checks and durable abandonment transition in one place.
#[allow(clippy::too_many_lines)]
pub(crate) fn acknowledge_preparation(paths: &Paths, id: &str) -> Result<Value> {
    let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, config) = configured(paths)?;
    ensure!(
        store.paused()?,
        "pause Telete before acknowledging preparation"
    );
    let mut job = store.job(id)?;
    let directory = paths.job(&job.id.0);
    let name = format!("production-{}", job.preparation_generation);
    let record = directory.join(format!("{name}.acknowledged.json"));
    let previous: Option<PreparationAcknowledgement> = if record.try_exists()? {
        Some(serde_json::from_slice(&fs::read(&record)?)?)
    } else {
        None
    };
    if let Some(acknowledgement) = &previous {
        ensure!(
            acknowledgement.schema == 1
                && acknowledgement.job == job.id
                && acknowledgement.operation.phase == Phase::Preparing
                && acknowledgement.operation.name == name
                && Some(&acknowledgement.operation.candidate) == job.candidate.as_ref()
                && acknowledgement.operation.result == directory.join(format!("{name}.json")),
            "preparation acknowledgement names another operation"
        );
        if job.phase == Phase::Failed && !job.unresolved && job.operation.is_none() {
            let worker = Worker {
                paths,
                store,
                config,
            };
            worker.cleanup(&mut job)?;
            return job.output();
        }
    }
    ensure!(
        job.phase == Phase::Blocked
            && job.stopped_phase == Some(Phase::Preparing)
            && job.outcome == Some(Phase::Failed)
            && job.unresolved
            && !job.model_unresolved
            && !job.accepted
            && job.preparation.is_none()
            && job.deployment_request.is_none()
            && job.deployment_result.is_none()
            && (!job.options.notify
                || job
                    .notification
                    .as_ref()
                    .is_some_and(|notice| { notice.accepted && !notice.uncertain })),
        "only an unaccepted blocked preparation can be acknowledged"
    );
    let operation = job
        .operation
        .as_ref()
        .context("preparation intent absent")?;
    ensure!(
        operation.phase == Phase::Preparing
            && operation.name == name
            && Some(&operation.candidate) == job.candidate.as_ref()
            && operation.result == directory.join(format!("{name}.json")),
        "preparation intent does not match the job"
    );
    ensure!(
        !operation.result.try_exists()?
            && !directory.join(&name).join("result.json").try_exists()?,
        "preparation has a terminal receipt; use recover"
    );
    ensure!(
        git::ref_value(paths, &config.repository, git::ACCEPTED)?.as_ref() == job.base.as_ref(),
        "accepted source changed after preparation"
    );
    crate::broker::require_settled(paths)?;
    if previous.is_none() {
        store::atomic_json(
            &record,
            &PreparationAcknowledgement {
                schema: 1,
                job: job.id.clone(),
                operation: operation.clone(),
                acknowledged: store::now(),
            },
        )?;
    } else {
        ensure!(
            serde_json::to_value(&previous.context("acknowledgement absent")?.operation)?
                == serde_json::to_value(operation)?,
            "retained preparation intent changed"
        );
    }
    job.operation = None;
    job.unresolved = false;
    job.phase = Phase::Failed;
    store.save(&mut job)?;
    let worker = Worker {
        paths,
        store,
        config,
    };
    worker.cleanup(&mut job)?;
    job.output()
}

pub(crate) async fn worker(paths: &Paths) -> Result<()> {
    let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
    let (store, config) = configured(paths)?;
    let mut worker = Worker {
        paths,
        store,
        config,
    };
    loop {
        worker.tick().await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

struct Worker<'a> {
    paths: &'a Paths,
    store: Store,
    config: Config,
}

impl Worker<'_> {
    fn directory(&self, job: &Job) -> Result<PathBuf> {
        let directory = self.paths.job(&job.id.0);
        store::private_directory(&directory)?;
        Ok(directory)
    }

    fn worktree(&self, job: &Job) -> PathBuf {
        self.paths.job(&job.id.0).join("worktree")
    }
    fn candidate(job: &Job) -> Result<&CommitId> {
        job.candidate
            .as_ref()
            .context("active job has no candidate")
    }
    fn base(job: &Job) -> Result<&CommitId> {
        job.base.as_ref().context("active job has no fixed base")
    }

    fn save(&self, job: &mut Job, phase: Phase) -> Result<()> {
        job.phase = phase;
        self.store.save(job)
    }

    fn claim(&self) -> Result<Option<Job>> {
        self.store.transaction(|| {
            if let Some(job) = self.store.active()? {
                return Ok(Some(job));
            }
            if self.store.paused()? || !self.store.holds()?.is_empty() {
                return Ok(None);
            }
            let Some(mut job) = self.store.queued()? else {
                return Ok(None);
            };
            self.paths.require_capacity()?;
            if job
                .options
                .workflow
                .as_ref()
                .is_none_or(|workflow| workflow.integrate)
            {
                job.base = Some(git::commit(
                    self.paths,
                    &self.config.repository,
                    self.config.accepted_ref(),
                )?);
            }
            self.save(&mut job, Phase::Integrating)?;
            Ok(Some(job))
        })
    }

    async fn tick(&mut self) -> Result<Value> {
        self.paths.require_capacity()?;
        for mut job in self.store.jobs()? {
            if (job.phase.terminal() || job.outcome.is_some())
                && !job.unresolved
                && !job.model_unresolved
            {
                self.cleanup(&mut job)?;
            }
        }
        let Some(mut job) = self.claim()? else {
            return Ok(json!({"state":"idle","paused":self.store.paused()?}));
        };
        if job.phase != Phase::Blocked
            && let Err(error) = self.step(&mut job).await
        {
            let message = format!("{error:#}");
            job.last_error = Some(message.clone());
            if job.phase == Phase::Notifying {
                job.phase = Phase::Blocked;
                self.store.set("paused", &true)?;
                self.store.save(&mut job)?;
            } else {
                let unresolved = job.model_unresolved
                    || job.operation.is_some()
                    || matches!(job.phase, Phase::Deploying);
                self.finish(&mut job, Phase::Failed, message, unresolved)?;
            }
        }
        self.store.job(&job.id.0)?.output()
    }

    async fn step(&mut self, job: &mut Job) -> Result<()> {
        job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
        if job.cancel_requested
            && job.operation.is_none()
            && !matches!(
                job.phase,
                Phase::RepairWait | Phase::Deploying | Phase::Notifying | Phase::Blocked
            )
        {
            return self.finish(
                job,
                Phase::Cancelled,
                "Cancelled before the next operation".into(),
                false,
            );
        }
        match job.phase {
            Phase::Integrating => self.integrating(job),
            Phase::Checking if job.options.workflow.is_some() => self.workflow_stage(job, "check"),
            Phase::Checking => self.checking(job),
            Phase::Autofixing => self.autofixing(job),
            Phase::RepairPrepare => self.repair_prepare(job),
            Phase::RepairWait => self.repair_wait(job).await,
            Phase::Applying => self.applying(job),
            Phase::Preparing if job.options.workflow.is_some() => {
                self.workflow_stage(job, "prepare")
            }
            Phase::Preparing => self.preparing(job),
            Phase::Accepting if job.options.workflow.is_some() => self.workflow_accept(job),
            Phase::Accepting => self.accepting(job),
            Phase::Deploying if job.options.workflow.is_some() => {
                self.workflow_stage(job, "deploy")
            }
            Phase::Deploying => self.deploying(job),
            Phase::Notifying => self.notifying(job).await,
            Phase::Blocked => Ok(()),
            _ => bail!("unexpected active Telete phase: {:?}", job.phase),
        }
    }

    fn assert_policy(&self, job: &Job) -> Result<()> {
        if job.options.workflow.is_some() {
            return Ok(());
        }
        ensure!(
            signing::selected(self.paths)? == *job.policy.signing()?,
            "host signing selection changed after admission"
        );
        Ok(())
    }

    fn finish(
        &self,
        job: &mut Job,
        outcome: Phase,
        message: String,
        unresolved: bool,
    ) -> Result<()> {
        ensure!(outcome.terminal(), "invalid terminal CI outcome");
        job.stopped_phase = Some(job.phase);
        job.outcome = Some(outcome);
        job.outcome_message = Some(message);
        job.unresolved = unresolved;
        if !matches!(outcome, Phase::Succeeded | Phase::AlreadyIncluded) {
            self.store.set("paused", &true)?;
        }
        if job.options.notify && job.notification.is_none() {
            let (subject, body) = render_notification(job);
            job.notification = Some(Notification {
                message: email::api::Message {
                    subject,
                    body,
                    idempotency_key: Some(format!(
                        "telete/{}/outcome/{}",
                        job.id, job.outcome_generation
                    )),
                },
                created: store::now(),
                attempts: Vec::new(),
                accepted: false,
                uncertain: false,
            });
        }
        let phase = if job.options.notify {
            Phase::Notifying
        } else if unresolved {
            Phase::Blocked
        } else {
            outcome
        };
        self.save(job, phase)?;
        self.cleanup(job)
    }

    fn cleanup(&self, job: &mut Job) -> Result<()> {
        if job.outcome.is_none()
            || job.unresolved
            || job.model_unresolved
            || job.cleanup.state == "removed"
        {
            return Ok(());
        }
        let worktree = self.worktree(job);
        if !job.cleanup.recorded {
            match git::registration(self.paths, &self.config.repository, &worktree) {
                Ok(registration) => {
                    job.cleanup.registration = registration;
                    job.cleanup.recorded = true;
                }
                Err(error) => {
                    job.cleanup.state = "failed".into();
                    job.cleanup.error = Some(format!("{error:#}"));
                    job.cleanup.updated = store::now();
                    return self.store.save(job);
                }
            }
            self.store.save(job)?;
        }
        match git::remove_worktree(
            self.paths,
            &self.config.repository,
            &worktree,
            job.cleanup.registration.as_deref(),
        ) {
            Ok(()) => {
                job.cleanup.state = "removed".into();
                job.cleanup.error = None;
            }
            Err(error) => {
                job.cleanup.state = "failed".into();
                job.cleanup.error = Some(format!("{error:#}"));
            }
        }
        job.cleanup.updated = store::now();
        self.store.save(job)
    }

    fn integrating(&self, job: &mut Job) -> Result<()> {
        if job
            .options
            .workflow
            .as_ref()
            .is_some_and(|workflow| !workflow.integrate)
        {
            let candidate = job.input.clone();
            job.candidate = Some(candidate.clone());
            self.store.save(job)?;
            git::advance(
                self.paths,
                &self.config.repository,
                &git::private_ref(&job.id.0, "candidate")?,
                None,
                &candidate,
            )?;
            git::ensure_worktree(
                self.paths,
                &self.config.repository,
                &self.worktree(job),
                &candidate,
            )?;
            return self.save(job, Phase::Checking);
        }
        let base = Self::base(job)?.clone();
        if job.options.workflow.is_none()
            && job.options.deploy.is_empty()
            && git::ancestor(self.paths, &self.config.repository, &job.input, &base)?
        {
            job.candidate = Some(base);
            return self.finish(
                job,
                Phase::AlreadyIncluded,
                "The submitted changes are already in accepted history".into(),
                false,
            );
        }
        let worktree = self.worktree(job);
        git::ensure_worktree(self.paths, &self.config.repository, &worktree, &base)?;
        let merge = git::merge(self.paths, &worktree, &job.input)?;
        store::atomic_bytes(
            &self.directory(job)?.join("merge.log"),
            format!("{}{}", merge.stdout, merge.stderr).as_bytes(),
        )?;
        if !merge.success() {
            return self.finish(
                job,
                Phase::Failed,
                "Integration failed. Merge conflicts require source changes".into(),
                false,
            );
        }
        let tree = git::value(self.paths, &worktree, &["write-tree"])?;
        let parents = if base == job.input {
            vec![base]
        } else {
            vec![base, job.input.clone()]
        };
        let candidate = git::commit_tree(
            self.paths,
            &self.config.repository,
            &tree,
            &parents,
            &job.id.0,
            job.created,
            "Integrate Telete submission",
        )?;
        job.candidate = Some(candidate.clone());
        self.store.save(job)?;
        git::advance(
            self.paths,
            &self.config.repository,
            &git::private_ref(&job.id.0, "candidate")?,
            None,
            &candidate,
        )?;
        git::ensure_worktree(self.paths, &self.config.repository, &worktree, &candidate)?;
        self.save(job, Phase::Checking)
    }

    fn begin_operation(&self, job: &mut Job, name: String, result: PathBuf) -> Result<bool> {
        let candidate = Self::candidate(job)?.clone();
        if let Some(operation) = &job.operation {
            ensure!(
                operation.name == name
                    && operation.candidate == candidate
                    && operation.phase == job.phase
                    && operation.result == result,
                "retained operation does not match current stage"
            );
            ensure!(
                result.is_file(),
                "interrupted operation has no terminal receipt; recovery is required"
            );
            return Ok(false);
        }
        ensure!(
            !result.exists(),
            "unattributed operation receipt already exists"
        );
        job.operation = Some(Operation {
            phase: job.phase,
            name,
            candidate,
            result,
            started: store::now(),
        });
        self.store.save(job)?;
        Ok(true)
    }

    // Receipt correlation, cancellation, and the next phase form one durable decision.
    #[allow(clippy::too_many_lines)]
    fn checking(&self, job: &mut Job) -> Result<()> {
        self.assert_policy(job)?;
        let candidate = Self::candidate(job)?.clone();
        let base = Self::base(job)?.clone();
        let worktree = self.worktree(job);
        let ordinal = job.validations.len();
        let directory = self.directory(job)?;
        let receipt = directory.join(format!("validation-{ordinal}.json"));
        let fresh = self.begin_operation(job, format!("validation-{ordinal}"), receipt.clone())?;
        if fresh {
            git::ensure_worktree(self.paths, &self.config.repository, &worktree, &candidate)?;
            let report = validation::run_candidate(
                self.paths,
                &worktree,
                &base,
                &candidate,
                job.options.run_tests,
                true,
                job,
            )?;
            store::atomic_json(&receipt, &report)?;
        }
        let report: ValidationReport = serde_json::from_slice(&fs::read(&receipt)?)?;
        validation::verify_report(&report, &base, &candidate, job.options.run_tests, true)?;
        validation::require_job_dispatcher(self.paths, &worktree, job)?;
        ensure!(
            report.schema == 1 && report.base == base && report.candidate == candidate,
            "validation receipt names another source"
        );
        if report.state == ValidationState::Passed {
            ensure!(
                report.tests_run == job.options.run_tests,
                "validation receipt has another test policy"
            );
        }
        if matches!(
            report.state,
            ValidationState::Passed | ValidationState::Autofix
        ) {
            ensure!(
                report.release_builds_deferred,
                "validation receipt has another release policy"
            );
        }
        job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
        if job.cancel_requested {
            let diagnostics = directory.join(format!("validation-{ordinal}.log"));
            store::atomic_bytes(&diagnostics, report.diagnostics.as_bytes())?;
            job.validations.push(Validation {
                report,
                diagnostics,
                stamp: store::now(),
                patch_content: None,
                fixed_candidate: None,
            });
            job.operation = None;
            return self.finish(
                job,
                Phase::Cancelled,
                "Cancelled after validation settled".into(),
                false,
            );
        }
        git::clean_candidate(self.paths, &worktree, &candidate)?;
        let inventory = Inventory::load(&worktree)?;
        let selected = inventory.select(&report.products)?;
        let names = selected
            .iter()
            .map(|product| product.id.to_string())
            .collect::<BTreeSet<_>>();
        ensure!(
            names.len() == report.products.len()
                && names == report.products.iter().cloned().collect(),
            "validation selection is not a canonical product scope"
        );
        let diagnostics = directory.join(format!("validation-{ordinal}.log"));
        store::atomic_bytes(&diagnostics, report.diagnostics.as_bytes())?;
        let patch_content = if report.state == ValidationState::Autofix {
            let path = report
                .autofix_patch
                .as_ref()
                .context("autofix receipt has no patch")?;
            ensure!(
                path.is_absolute() && path.starts_with(&self.paths.root),
                "autofix patch is outside Telete state"
            );
            let raw = fs::read_to_string(path)?;
            ensure!(!raw.is_empty(), "autofix patch is empty");
            Some(raw)
        } else {
            None
        };
        job.validations.push(Validation {
            report: report.clone(),
            diagnostics,
            stamp: store::now(),
            patch_content,
            fixed_candidate: None,
        });
        job.operation = None;
        job.production_diagnostics = None;
        job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
        if job.cancel_requested {
            return self.finish(
                job,
                Phase::Cancelled,
                "Cancelled after validation settled".into(),
                false,
            );
        }
        match report.state {
            ValidationState::Passed => self.save(job, Phase::Preparing),
            ValidationState::Autofix => self.save(job, Phase::Autofixing),
            ValidationState::Failed => self.save(job, Phase::RepairPrepare),
            ValidationState::Stale => self.finish(
                job,
                Phase::Failed,
                "Validation became stale; fresh validation requires a new submission".into(),
                true,
            ),
        }
    }

    fn autofixing(&self, job: &mut Job) -> Result<()> {
        self.assert_policy(job)?;
        let validation = job
            .validations
            .last()
            .context("autofix has no validation")?;
        let parent = validation.report.candidate.clone();
        ensure!(
            validation.report.state == ValidationState::Autofix && Self::candidate(job)? == &parent,
            "autofix names another candidate"
        );
        let raw = validation
            .patch_content
            .as_ref()
            .context("autofix patch was not retained")?;
        let path = validation
            .report
            .autofix_patch
            .as_ref()
            .context("autofix path absent")?;
        ensure!(
            fs::read_to_string(path)? == *raw,
            "retained autofix patch changed"
        );
        let tree = git::patch_tree(
            self.paths,
            &self.config.repository,
            &parent,
            raw,
            &self.directory(job)?.join("autofix.index"),
        )?;
        ensure!(
            tree != git::value(
                self.paths,
                &self.config.repository,
                &["rev-parse", &format!("{parent}^{{tree}}")]
            )?,
            "autofix made no source changes"
        );
        let candidate = git::commit_tree(
            self.paths,
            &self.config.repository,
            &tree,
            std::slice::from_ref(&parent),
            &job.id.0,
            validation.stamp,
            "Apply deterministic Telete fixes",
        )?;
        if let Some(recorded) = &validation.fixed_candidate {
            ensure!(recorded == &candidate, "autofix candidate changed");
        }
        job.validations
            .last_mut()
            .context("autofix record absent")?
            .fixed_candidate = Some(candidate.clone());
        self.store.save(job)?;
        git::advance(
            self.paths,
            &self.config.repository,
            &git::private_ref(&job.id.0, "candidate")?,
            Some(&parent),
            &candidate,
        )?;
        job.candidate = Some(candidate.clone());
        git::ensure_worktree(
            self.paths,
            &self.config.repository,
            &self.worktree(job),
            &candidate,
        )?;
        self.save(job, Phase::Checking)
    }

    // Freeze the complete request before recording its single charged invocation.
    #[allow(clippy::too_many_lines)]
    fn repair_prepare(&self, job: &mut Job) -> Result<()> {
        self.assert_policy(job)?;
        if !job.options.repair {
            return self.finish(
                job,
                Phase::Failed,
                "Required checks failed; automatic repair is disabled".into(),
                false,
            );
        }
        let used = job.charged_attempts() as u64;
        if used >= job.policy.primary_attempts + job.policy.escalation_attempts {
            return self.finish(
                job,
                Phase::Failed,
                "Automatic repair exhausted its unrefunded budget".into(),
                false,
            );
        }
        let _admission = store::lock(&self.paths.root.join("admission.lock"), true)?;
        if !self.store.holds()?.is_empty() {
            job.waiting_reason = Some("requester_maintenance".into());
            return self.store.save(job);
        }
        let number = job.attempts.len() as u64 + 1;
        let identity = format!("telete-{}-repair-{number}", job.id);
        let diagnostics = job
            .production_diagnostics
            .as_ref()
            .or_else(|| {
                job.validations
                    .last()
                    .map(|validation| &validation.diagnostics)
            })
            .context("repair has no diagnostics")?;
        let parent = Self::candidate(job)?.clone();
        let history = job
            .attempts
            .iter()
            .map(|attempt| {
                json!({"number":attempt.number,
            "nucleus_job_id":attempt.request.id,"parent":attempt.parent,"state":attempt.state,
            "candidate":attempt.candidate,"rejection":attempt.rejection})
            })
            .collect::<Vec<_>>();
        let prompts = job
            .policy
            .prompts
            .as_ref()
            .context("repair-enabled job has no frozen prompt")?;
        let components = BTreeMap::from([
            (
                prompts.instructions.id.clone(),
                prompts.instructions.version,
            ),
            (prompts.template.id.clone(), prompts.template.version),
        ]);
        let context = json!({"domain_job":job.id,"accepted_base":job.base,"candidate_commit":parent,
            "candidate_directory":self.worktree(job),"diagnostic_path":diagnostics,"prior_attempts":history,
            "prompt_selection":{"id":prompts.selection.id,"version":prompts.selection.version,
                "components":components}});
        let primary = used < job.policy.primary_attempts;
        let request = providers::request(
            &identity,
            &job.id.0,
            &self.worktree(job),
            &context,
            job.policy
                .prompts
                .as_ref()
                .context("repair-enabled job has no frozen prompt")?,
            if primary {
                &job.policy.primary_model
            } else {
                &job.policy.escalation_model
            },
            if primary {
                job.policy.primary_reasoning
            } else {
                job.policy.escalation_reasoning
            },
            job.policy.model_timeout_seconds,
        )?;
        let directory = self.directory(job)?;
        store::atomic_json(
            &directory.join(format!("repair-{number}.request.json")),
            &request,
        )?;
        job.attempts.push(RepairAttempt {
            number,
            request,
            parent,
            stamp: store::now(),
            state: "prepared".into(),
            patch: None,
            patch_content: None,
            candidate: None,
            rejection: None,
            transport_failures: 0,
            terminal: None,
        });
        job.model_unresolved = true;
        job.waiting_reason = None;
        self.save(job, Phase::RepairWait)
    }

    // Keep one frozen request's observations and admission effects in one workflow.
    #[allow(clippy::too_many_lines)]
    async fn repair_wait(&self, job: &mut Job) -> Result<()> {
        let attempt = job
            .attempts
            .last()
            .context("repair wait has no saved attempt")?
            .clone();
        let saved: nucleus_core::JobRequestV1 = serde_json::from_slice(&fs::read(
            self.directory(job)?
                .join(format!("repair-{}.request.json", attempt.number)),
        )?)?;
        ensure!(saved == attempt.request, "saved Nucleus request changed");
        let client = providers::client(&job.policy.providers)?;
        let observed = providers::get_job(&client, &attempt.request.id).await;
        match observed {
            Ok(view) => {
                let observation = providers::correlate(&view, &attempt.request)?;
                job.attempts
                    .last_mut()
                    .context("attempt missing")?
                    .transport_failures = 0;
                job.model_unresolved = true;
                job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
                if job.cancel_requested && !view.summary.state.is_terminal() {
                    let cancelled = match providers::cancel(&client, &attempt.request.id).await {
                        Ok(cancelled) => cancelled,
                        Err(error) => return self.transport_failure(job, &error),
                    };
                    ensure!(
                        cancelled.version == 1 && cancelled.job_id == attempt.request.id,
                        "Nucleus cancellation names another job"
                    );
                    job.waiting_reason = Some("cancelling_nucleus".into());
                    return self.store.save(job);
                }
                match observation {
                    Observation::Pending => {
                        job.waiting_reason = Some(
                            if view
                                .quota
                                .as_ref()
                                .is_some_and(nucleus_core::QuotaStatusV1::is_blocked)
                            {
                                "quota_deferred"
                            } else {
                                "nucleus"
                            }
                            .into(),
                        );
                        self.store.save(job)
                    }
                    Observation::Failed { lost, message } => {
                        store::atomic_json(
                            &self
                                .directory(job)?
                                .join(format!("repair-{}.result.json", attempt.number)),
                            &view,
                        )?;
                        let current = job.attempts.last_mut().context("attempt missing")?;
                        current.state = if lost { "lost" } else { "failed" }.into();
                        current.terminal = Some(serde_json::to_value(view)?);
                        job.model_unresolved = lost;
                        self.finish(
                            job,
                            if job.cancel_requested && !lost {
                                Phase::Cancelled
                            } else {
                                Phase::Failed
                            },
                            message,
                            lost,
                        )
                    }
                    Observation::Completed(patch) => {
                        store::atomic_json(
                            &self
                                .directory(job)?
                                .join(format!("repair-{}.result.json", attempt.number)),
                            &view,
                        )?;
                        job.model_unresolved = false;
                        let current = job.attempts.last_mut().context("attempt missing")?;
                        current.state = "completed".into();
                        current.terminal = Some(serde_json::to_value(view)?);
                        if job.cancel_requested {
                            return self.finish(
                                job,
                                Phase::Cancelled,
                                "Cancelled after model execution settled".into(),
                                false,
                            );
                        }
                        if let Some(raw) = patch {
                            let patch_file = self
                                .directory(job)?
                                .join(format!("repair-{}.patch", attempt.number));
                            store::atomic_bytes(&patch_file, raw.as_bytes())?;
                            let current = job.attempts.last_mut().context("attempt missing")?;
                            current.patch = Some(patch_file);
                            current.patch_content = Some(raw);
                            self.save(job, Phase::Applying)
                        } else {
                            job.attempts
                                .last_mut()
                                .context("attempt missing")?
                                .rejection = Some("No correlated final patch was returned".into());
                            self.save(job, Phase::RepairPrepare)
                        }
                    }
                }
            }
            Err(error) if providers::absent(&error) => {
                let _admission = store::lock(&self.paths.root.join("admission.lock"), true)?;
                job.model_unresolved = false;
                job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
                if job.cancel_requested {
                    return self.finish(
                        job,
                        Phase::Cancelled,
                        "Cancelled before model admission".into(),
                        false,
                    );
                }
                if !self.store.holds()?.is_empty() {
                    job.waiting_reason = Some("requester_maintenance".into());
                    return self.store.save(job);
                }
                match providers::health(&client).await {
                    Ok(health) => ensure!(
                        health.version == 1
                            && health.status == "ok"
                            && health.accepting_jobs
                            && health.supported_protocol_versions.contains(&1)
                            && health.authentication.authenticated,
                        "Nucleus strict readiness was not proved"
                    ),
                    Err(error) if error.deferred() => {
                        job.waiting_reason = Some("quota_deferred".into());
                        return self.store.save(job);
                    }
                    Err(error) => return self.transport_failure(job, &error),
                }
                job.model_unresolved = true;
                self.store.save(job)?;
                match providers::submit(&client, &attempt.request).await {
                    Ok(accepted) => {
                        ensure!(
                            accepted.version == 1 && accepted.job_id == attempt.request.id,
                            "Nucleus admission names another request"
                        );
                        ensure!(
                            accepted.request_digest == attempt.request.request_digest()?,
                            "Nucleus admission has another request digest"
                        );
                        job.attempts.last_mut().context("attempt missing")?.state =
                            "submitted".into();
                        job.attempts
                            .last_mut()
                            .context("attempt missing")?
                            .transport_failures = 0;
                        job.waiting_reason = Some("nucleus".into());
                        self.store.save(job)
                    }
                    Err(error) if error.deferred() => {
                        job.model_unresolved = false;
                        job.waiting_reason = Some("quota_deferred".into());
                        self.store.save(job)
                    }
                    Err(error) => self.transport_failure(job, &error),
                }
            }
            Err(error) => self.transport_failure(job, &error),
        }
    }

    fn transport_failure(&self, job: &mut Job, error: &providers::CallError) -> Result<()> {
        job.waiting_reason = Some("nucleus_transport".into());
        job.last_error = Some(error.to_string());
        let attempt = job
            .attempts
            .last_mut()
            .context("transport failure has no saved attempt")?;
        attempt.transport_failures += 1;
        if attempt.transport_failures >= 5 {
            self.finish(
                job,
                Phase::Failed,
                "Nucleus observation remains unavailable; the exact request identity is retained"
                    .into(),
                true,
            )
        } else {
            self.store.save(job)
        }
    }

    fn applying(&self, job: &mut Job) -> Result<()> {
        self.assert_policy(job)?;
        let attempt = job
            .attempts
            .last()
            .context("patch application has no attempt")?
            .clone();
        ensure!(
            Self::candidate(job)? == &attempt.parent,
            "patch parent differs from current candidate"
        );
        let raw = attempt
            .patch_content
            .as_ref()
            .context("patch content was not retained")?;
        ensure!(
            fs::read_to_string(attempt.patch.as_ref().context("patch path absent")?)? == *raw,
            "saved model patch changed"
        );
        let tree = match git::patch_tree(
            self.paths,
            &self.config.repository,
            &attempt.parent,
            raw,
            &self.directory(job)?.join("repair.index"),
        ) {
            Ok(tree) => tree,
            Err(error) => {
                job.attempts.last_mut().context("attempt absent")?.rejection =
                    Some(error.to_string());
                return self.save(job, Phase::RepairPrepare);
            }
        };
        let candidate = git::commit_tree(
            self.paths,
            &self.config.repository,
            &tree,
            std::slice::from_ref(&attempt.parent),
            &job.id.0,
            attempt.stamp,
            &format!("Apply Telete repair {}", attempt.number),
        )?;
        if let Some(previous) = attempt.candidate {
            ensure!(previous == candidate, "recorded repair candidate changed");
        }
        job.attempts.last_mut().context("attempt absent")?.candidate = Some(candidate.clone());
        self.store.save(job)?;
        git::advance(
            self.paths,
            &self.config.repository,
            &git::private_ref(&job.id.0, "candidate")?,
            Some(&attempt.parent),
            &candidate,
        )?;
        job.candidate = Some(candidate.clone());
        git::ensure_worktree(
            self.paths,
            &self.config.repository,
            &self.worktree(job),
            &candidate,
        )?;
        self.save(job, Phase::Checking)
    }

    fn production_products(&self, job: &Job) -> Result<Vec<String>> {
        cell_pipeline::production_products(self.paths, job)
    }

    fn deployment_products(&self, job: &Job) -> Result<Vec<String>> {
        if job.options.no_deploy {
            return Ok(Vec::new());
        }
        let names = if job.options.deploy.is_empty() {
            let report = &job
                .validations
                .last()
                .context("deployment has no validation")?
                .report;
            let mut names = report.products.clone();
            names.extend(report.platform_products.iter().cloned());
            names.retain(|product| product != "telete");
            names
        } else {
            job.options.deploy.clone()
        };
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let selected = Inventory::load(&self.worktree(job))?.select(&names)?;
        ensure!(
            !selected
                .iter()
                .any(|product| product.id.to_string() == "telete"),
            "Telete installation is a separate explicit operation"
        );
        let mut canonical = selected
            .iter()
            .map(|product| {
                if product.id.to_string() == "decisions" {
                    "krisis".into()
                } else {
                    product.id.to_string()
                }
            })
            .collect::<Vec<_>>();
        canonical.sort();
        canonical.dedup();
        Ok(canonical)
    }

    fn preparing(&self, job: &mut Job) -> Result<()> {
        self.assert_policy(job)?;
        let candidate = Self::candidate(job)?.clone();
        let products = self.production_products(job)?;
        let directory = self.directory(job)?;
        let generation = job.preparation_generation;
        let receipt = directory.join(format!("production-{generation}.json"));
        let fresh =
            self.begin_operation(job, format!("production-{generation}"), receipt.clone())?;
        if fresh {
            if products.is_empty() {
                store::atomic_json(
                    &receipt,
                    &PreparationOutcome {
                        prepared: Some(Preparation {
                            schema: 1,
                            source: candidate.clone(),
                            products: Vec::new(),
                            signing_policy: job.policy.signing()?.clone(),
                            candidates: BTreeMap::new(),
                            release_check: true,
                        }),
                        compilation_failure: None,
                    },
                )?;
            } else {
                cell_pipeline::prepare(self.paths, job)?;
            }
        }
        let outcome: PreparationOutcome = serde_json::from_slice(&fs::read(receipt)?)?;
        ensure!(
            outcome.prepared.is_some() != outcome.compilation_failure.is_some(),
            "production outcome is inconsistent"
        );
        job.operation = None;
        job.preparation_generation += 1;
        if let Some(diagnostic) = outcome.compilation_failure {
            let path = directory.join(format!("production-{generation}.log"));
            store::atomic_bytes(&path, diagnostic.as_bytes())?;
            job.production_diagnostics = Some(path);
            job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
            if job.cancel_requested {
                return self.finish(
                    job,
                    Phase::Cancelled,
                    "Cancelled after release compilation settled".into(),
                    false,
                );
            }
            return self.save(job, Phase::RepairPrepare);
        }
        let prepared = outcome
            .prepared
            .context("production receipt has no prepared result")?;
        correlate_preparation(
            &prepared,
            &candidate,
            &products,
            job.policy.signing()?,
            self.paths,
        )?;
        job.preparation = Some(prepared);
        job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
        if job.cancel_requested {
            return self.finish(
                job,
                Phase::Cancelled,
                "Cancelled after production preparation settled".into(),
                false,
            );
        }
        self.save(job, Phase::Accepting)
    }

    fn accepting(&self, job: &mut Job) -> Result<()> {
        self.assert_policy(job)?;
        let candidate = Self::candidate(job)?.clone();
        let products = self.production_products(job)?;
        let prepared = job
            .preparation
            .as_ref()
            .context("acceptance has no production receipt")?
            .clone();
        correlate_preparation(
            &prepared,
            &candidate,
            &products,
            job.policy.signing()?,
            self.paths,
        )?;
        git::clean_candidate(self.paths, &self.worktree(job), &candidate)?;
        git::advance(
            self.paths,
            &self.config.repository,
            git::ACCEPTED,
            Some(Self::base(job)?),
            &candidate,
        )?;
        job.accepted = true;
        self.store.save(job)?;
        let deploy_products = self.deployment_products(job)?;
        if deploy_products.is_empty() {
            return self.finish(
                job,
                Phase::Succeeded,
                "Required checks passed; no deployment was selected".into(),
                false,
            );
        }
        if job.deployment_request.is_none() {
            job.deployment_request = Some(DeploymentRequest {
                id: format!("telete-{}-deployment", job.id),
                source: candidate,
                products: deploy_products,
                prepared: prepared.clone(),
            });
        }
        self.save(job, Phase::Deploying)
    }

    fn deploying(&self, job: &mut Job) -> Result<()> {
        self.assert_policy(job)?;
        let request = job
            .deployment_request
            .as_ref()
            .context("deployment has no frozen request")?
            .clone();
        ensure!(
            git::ref_value(self.paths, &self.config.repository, git::ACCEPTED)?.as_ref()
                == Some(&request.source),
            "accepted source changed before deployment"
        );
        let observed = deployment::observe(self.paths, &request.id)?;
        if observed.is_none() && job.cancel_requested {
            return self.finish(
                job,
                Phase::Cancelled,
                "Cancelled after source acceptance and before deployment admission".into(),
                false,
            );
        }
        let result = match observed {
            Some(result) => result,
            None => match cell_pipeline::deploy(self.paths, job) {
                Ok(result) => result,
                Err(error) => match deployment::observe(self.paths, &request.id)? {
                    Some(result) => result,
                    None => {
                        return self.finish(
                            job,
                            Phase::Failed,
                            format!("Deployment stopped before admission: {error}"),
                            false,
                        );
                    }
                },
            },
        };
        correlate_deployment(&result, &request)?;
        store::atomic_json(&self.directory(job)?.join("deployment.json"), &result)?;
        job.deployment_result = Some(result.clone());
        match result.state {
            DeploymentState::Succeeded => self.finish(
                job,
                Phase::Succeeded,
                "Deployment instructions completed for the accepted candidate".into(),
                false,
            ),
            DeploymentState::Failed => self.finish(
                job,
                Phase::Failed,
                "Deployment instructions failed; accepted source is retained".into(),
                false,
            ),
            DeploymentState::Interrupted if result.acknowledged => self.finish(
                job,
                Phase::Failed,
                "Interrupted deployment was explicitly acknowledged; no instruction was repeated"
                    .into(),
                false,
            ),
            DeploymentState::Interrupted | DeploymentState::Running => self.finish(
                job,
                Phase::Failed,
                "Deployment instruction effects remain unresolved".into(),
                true,
            ),
        }
    }

    async fn notifying(&self, job: &mut Job) -> Result<()> {
        let notice = job
            .notification
            .as_ref()
            .context("notification phase has no frozen message")?;
        if notice.accepted {
            return self.save(
                job,
                if job.unresolved {
                    Phase::Blocked
                } else {
                    job.outcome.context("notification has no CI outcome")?
                },
            );
        }
        let now = store::now();
        if notice
            .attempts
            .last()
            .is_some_and(|attempt| now.saturating_sub(attempt.started) < 300)
        {
            return Ok(());
        }
        if notice.attempts.len() >= 2 || now.saturating_sub(notice.created) >= 23 * 3600 {
            job.notification
                .as_mut()
                .context("notice absent")?
                .uncertain = true;
            self.store.set("paused", &true)?;
            return self.save(job, Phase::Blocked);
        }
        let message = notice.message.clone();
        job.notification
            .as_mut()
            .context("notice absent")?
            .attempts
            .push(SendAttempt {
                started: now,
                receipt: None,
                error: None,
            });
        self.store.save(job)?;
        let result = providers::email(&job.policy.providers, &message).await;
        let notice = job.notification.as_mut().context("notice absent")?;
        let attempt = notice.attempts.last_mut().context("send attempt absent")?;
        match result {
            Ok(receipt) => {
                attempt.receipt = Some(receipt);
                notice.accepted = true;
            }
            Err(error) => attempt.error = Some(error.to_string()),
        }
        self.store.save(job)
    }
}

fn correlate_preparation(
    prepared: &Preparation,
    candidate: &CommitId,
    products: &[String],
    policy: &SigningPolicy,
    paths: &Paths,
) -> Result<()> {
    ensure!(
        prepared.schema == 1
            && prepared.source == *candidate
            && prepared.products == products
            && prepared.signing_policy == *policy
            && prepared.release_check,
        "production preparation does not match source, scope, signing policy or release checks"
    );
    ensure!(
        prepared.candidates.keys().cloned().collect::<BTreeSet<_>>()
            == products.iter().cloned().collect(),
        "prepared candidate scope is incomplete"
    );
    for (product, artifact) in &prepared.candidates {
        ensure!(
            artifact.schema == 1
                && artifact.product == *product
                && artifact.source_commit == *candidate
                && artifact.signing_policy == *policy
                && artifact.source_key == candidate.to_string()
                && !artifact.candidate_id.is_empty()
                && artifact.candidate_dir.is_absolute()
                && artifact.candidate_dir.starts_with(&paths.root),
            "production candidate correlation is invalid"
        );
    }
    Ok(())
}

fn correlate_deployment(result: &DeploymentResult, request: &DeploymentRequest) -> Result<()> {
    let result_products = result.products.iter().collect::<BTreeSet<_>>();
    let request_products = request.products.iter().collect::<BTreeSet<_>>();
    ensure!(
        result.schema == 2
            && result.request_id == request.id
            && result.source == request.source
            && result_products.len() == result.products.len()
            && request_products.len() == request.products.len()
            && result_products == request_products,
        "deployment receipt does not match admitted request"
    );
    ensure!(
        !result.run_id.is_empty(),
        "deployment receipt has no owning run identity"
    );
    if result.state == DeploymentState::Succeeded {
        ensure!(
            result.exit_code == Some(0)
                && !result.instructions.is_empty()
                && result
                    .instructions
                    .iter()
                    .all(
                        |instruction| instruction.state == InstructionState::Succeeded
                            && instruction.exit_code == Some(0)
                    ),
            "deployment success has incomplete instruction results"
        );
    }
    Ok(())
}

fn render_notification(job: &Job) -> (String, String) {
    let products = job
        .deployment_result
        .as_ref()
        .map(|result| result.products.clone())
        .or_else(|| {
            job.validations
                .last()
                .map(|validation| validation.report.products.clone())
        })
        .unwrap_or_default();
    let scope = products
        .iter()
        .map(|name| name.replace('-', " "))
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if scope.is_empty() {
        String::new()
    } else {
        format!(" — {scope}")
    };
    let (title, mut body) = match job.outcome {
        Some(Phase::Succeeded) if job.options.workflow.is_some() => {
            let deployed = job.workflow_runs.iter().any(|run| {
                run.stage == "deploy"
                    && Some(&run.candidate) == job.candidate.as_ref()
                    && run.result.success()
            });
            (
                if deployed { "deployed" } else { "passed" },
                if deployed {
                    "Project checks and selected deployment command completed."
                } else {
                    "Requested project workflow stages completed."
                }
                .to_owned(),
            )
        }
        Some(Phase::Succeeded)
            if job
                .deployment_result
                .as_ref()
                .is_some_and(|result| result.state == DeploymentState::Succeeded) =>
        {
            (
                "deployed",
                "Required checks passed and deployment instructions completed.".to_owned(),
            )
        }
        Some(Phase::Succeeded) => (
            "passed",
            "Required checks passed. No deployment was selected.".to_owned(),
        ),
        Some(Phase::AlreadyIncluded) => (
            "already included",
            "These changes are already in accepted history. No new checks or deployment ran."
                .to_owned(),
        ),
        Some(Phase::Cancelled) => (
            "cancelled",
            "CI was cancelled. Recorded source acceptance and completed effects are retained."
                .to_owned(),
        ),
        _ => {
            let phase = job
                .stopped_phase
                .map_or("processing", Phase::name)
                .replace('_', " ");
            let detail = notification_detail(
                job,
                job.outcome_message
                    .as_deref()
                    .unwrap_or("No specific cause was reported."),
            );
            (
                "failed",
                format!("What failed: {phase}.\n{detail}\nThe Telete queue is paused."),
            )
        }
    };
    if job.options.workflow.is_none() && !job.options.run_tests {
        body.push_str("\n\nTests were skipped.");
    }
    if job.unresolved {
        body.push_str("\nThe recorded operation requires recovery.");
    }
    (format!("Telete: {title}{suffix}"), body)
}

fn notification_detail(job: &Job, value: &str) -> String {
    let first = value.lines().next().unwrap_or("");
    let mut words = Vec::new();
    for word in first.split_whitespace() {
        let reference = word.contains('/')
            || word.contains('\\')
            || word.contains(&job.id.0)
            || [
                &job.input,
                job.base.as_ref().unwrap_or(&job.input),
                job.candidate.as_ref().unwrap_or(&job.input),
            ]
            .iter()
            .any(|id| word.contains(&id.0))
            || word.starts_with("gpt-");
        if reference {
            if words
                .last()
                .is_none_or(|last| last != "[local details omitted]")
            {
                words.push("[local details omitted]".to_owned());
            }
        } else {
            words.push(word.to_owned());
        }
    }
    words.join(" ").chars().take(350).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing::MacSigning;
    use bazaar::api::Record;
    use std::os::unix::fs::PermissionsExt;

    fn policy(paths: &Paths) -> Result<Policy> {
        let signing_policy = SigningPolicy {
            schema: 1,
            macos: MacSigning {
                profile: "local".into(),
                certificate_sha1: "a".repeat(40),
                keychain: paths.root.join("fixture.keychain"),
                identifier_namespace: "local.fixture".into(),
            },
        };
        store::atomic_json(&paths.root.join("signing.json"), &signing_policy)?;
        let record = |id: &str, content: &str| Record {
            id: id.into(),
            version: 1,
            content: content.into(),
        };
        Ok(Policy {
            commands: None,
            primary_attempts: 3,
            escalation_attempts: 1,
            primary_model: "gpt-5.6-terra".into(),
            primary_reasoning: nucleus_core::ReasoningEffort::Medium,
            escalation_model: "gpt-5.6-sol".into(),
            escalation_reasoning: nucleus_core::ReasoningEffort::High,
            model_timeout_seconds: 600,
            signing_policy: Some(signing_policy),
            providers: Selection {
                nucleus_socket: Some(paths.root.join("not-a-live-socket")),
                email_executable: None,
                bazaar_database: Some(paths.root.join("not-a-live-bazaar")),
            },
            prompts: Some(Prompts {
                selection: record(providers::PROMPT_SELECTION, "{}"),
                instructions: record(providers::PROMPT_INSTRUCTIONS, "Return a patch."),
                template: record(providers::PROMPT_TEMPLATE, "{context}"),
            }),
        })
    }

    fn fixture() -> Result<(tempfile::TempDir, Paths, PathBuf, CommitId)> {
        let temporary = tempfile::tempdir()?;
        let paths = Paths::for_test(temporary.path().join("telete"))?;
        let repo = temporary.path().join("repository");
        fs::create_dir(&repo)?;
        git::value(&paths, &repo, &["init", "--initial-branch=main"])?;
        fs::write(repo.join("file.txt"), "base\n")?;
        fs::create_dir_all(repo.join("pipeline/products"))?;
        for product in ["alpha", "beta", "decisions"] {
            fs::create_dir_all(repo.join(format!("products/{product}")))?;
            fs::write(repo.join(format!("products/{product}/marker")), product)?;
            fs::write(
                repo.join(format!("pipeline/products/{product}.sh")),
                format!(
                    "PIPELINE_SCHEMA=1\nPRODUCT_ID={product}\nPRODUCT_NAME={product}\nPRODUCT_DIR=products/{product}\nCARGO_PACKAGES={product}\nPROVIDERS='one|{product}|products/{product}/chancery|{product}'\n{}",
                    if product == "decisions" {
                        "PRODUCT_ALIASES=krisis\n"
                    } else {
                        ""
                    }
                ),
            )?;
        }
        fs::create_dir_all(repo.join("infrastructure/telete"))?;
        fs::write(
            repo.join("infrastructure/telete/product.sh"),
            "PIPELINE_SCHEMA=1\nPRODUCT_ID=telete\nPRODUCT_NAME=Telete\nPRODUCT_DIR=infrastructure/telete\nCARGO_PACKAGES=telete\nPROVIDERS='one|telete|infrastructure/telete/chancery|telete'\n",
        )?;
        git::value(&paths, &repo, &["add", "."])?;
        let tree = git::value(&paths, &repo, &["write-tree"])?;
        let base = git::commit_tree(
            &paths,
            &repo,
            &tree,
            &[],
            "fixture",
            1_700_000_000,
            "Fixture baseline",
        )?;
        git::value(&paths, &repo, &["update-ref", "HEAD", &base.0])?;
        init(&paths, &repo, Some(&base.0))?;
        Ok((temporary, paths, repo, base))
    }

    fn job(paths: &Paths, base: &CommitId, id: &str) -> Result<Job> {
        let options = SubmitOptions {
            notify: false,
            repair: false,
            ..SubmitOptions::default()
        };
        Ok(new_job(
            JobId::new(id)?,
            format!("request-{id}"),
            base.clone(),
            options,
            policy(paths)?,
        ))
    }

    fn worker(paths: &Paths) -> Result<Worker<'_>> {
        let (store, config) = configured(paths)?;
        Ok(Worker {
            paths,
            store,
            config,
        })
    }

    fn report(base: &CommitId, candidate: &CommitId, products: &[&str]) -> ValidationReport {
        ValidationReport {
            schema: 1,
            base: base.clone(),
            candidate: candidate.clone(),
            state: ValidationState::Passed,
            products: products.iter().map(|value| (*value).into()).collect(),
            platform_products: Vec::new(),
            shared_suites: Vec::new(),
            tests_run: false,
            release_builds_deferred: true,
            required_gates: Vec::new(),
            gates: Vec::new(),
            diagnostics: String::new(),
            autofix_patch: None,
        }
    }

    fn retain_check_dispatcher(
        paths: &Paths,
        worktree: &Path,
        base: &CommitId,
        run_tests: bool,
        exit_code: Option<i32>,
    ) -> Result<()> {
        let key = validation::check_dispatcher_key(base, base, run_tests, true);
        let command =
            validation::check_dispatcher_spec(paths, worktree, base, base, run_tests, true);
        let record = paths
            .root
            .join("broker")
            .join(format!("{}.json", key.replace('/', "_")));
        let process = record.with_extension("process");
        let result = exit_code.map(|exit_code| crate::process::ProcessResult {
            start_error: None,
            signal: None,
            output_error: None,
            exit_code: Some(exit_code),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        });
        store::atomic_json(
            &record,
            &json!({"schema":1,"key":key,"class":"supervisor","command":command,
                "state":if result.is_some() {"finished"} else {"running"},
                "owner_pid":1,"result":result}),
        )?;
        store::atomic_json(&process.join("request.json"), &command)?;
        if let Some(result) = result {
            store::atomic_json(
                &process.join("result.json"),
                &json!({"schema":1,"request":command,"result":result}),
            )?;
        }
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn frozen_check_invocations_follow_each_candidate_and_reject_changed_correlation() -> Result<()>
    {
        let (_temporary, paths, repo, base) = fixture()?;
        let mut job = job(&paths, &base, "frozen-check")?;
        let mut commands = CommandPolicy::capture(&paths)?;
        commands
            .environment
            .insert("TELETE_TEST_FROZEN".into(), "retained".into());
        job.policy.commands = Some(commands);
        job.base = Some(base.clone());
        let script = repo.join("pipeline/cell/check.sh");
        fs::create_dir_all(script.parent().context("check script parent")?)?;
        let candidate = |parent: &CommitId, exit_code: i32| -> Result<CommitId> {
            fs::write(
                &script,
                format!("[ \"$TELETE_TEST_FROZEN\" = retained ] || exit 99\nexit {exit_code}\n"),
            )?;
            git::value(&paths, &repo, &["add", "pipeline/cell/check.sh"])?;
            let tree = git::value(&paths, &repo, &["write-tree"])?;
            git::commit_tree(
                &paths,
                &repo,
                &tree,
                std::slice::from_ref(parent),
                "frozen-check",
                1_700_000_001,
                "Candidate check script",
            )
        };
        let first = candidate(&base, 0)?;
        job.candidate = Some(first.clone());
        let worktree = paths.job(&job.id.0).join("worktree");
        git::ensure_worktree(&paths, &repo, &worktree, &first)?;
        let first_key = validation::job_dispatcher_key(&job)?;
        let first_command = validation::job_dispatcher_spec(&paths, &worktree, &job)?;
        assert!(
            crate::broker::run(
                &paths,
                &first_key,
                crate::broker::ResourceClass::Supervisor,
                &first_command,
            )?
            .success()
        );
        validation::require_job_dispatcher(&paths, &worktree, &job)?;

        let mut changed = job.clone();
        changed.options.run_tests = true;
        assert!(validation::require_job_dispatcher(&paths, &worktree, &changed).is_err());
        let mut changed = job.clone();
        changed
            .policy
            .commands
            .as_mut()
            .context("frozen commands")?
            .environment
            .insert("TELETE_TEST_FROZEN".into(), "changed".into());
        assert!(validation::require_job_dispatcher(&paths, &worktree, &changed).is_err());
        let record = paths
            .root
            .join("broker")
            .join(format!("{}.json", first_key.replace('/', "_")));
        let saved = fs::read(&record)?;
        let mut changed: Value = serde_json::from_slice(&saved)?;
        changed["command"]["context"]["value"]["source"]["candidate"] = json!(base);
        store::atomic_json(&record, &changed)?;
        assert!(validation::require_job_dispatcher(&paths, &worktree, &job).is_err());
        store::atomic_bytes(&record, &saved)?;

        let second = candidate(&first, 9)?;
        let mut repaired = job.clone();
        repaired.candidate = Some(second.clone());
        assert!(validation::require_job_dispatcher(&paths, &worktree, &repaired).is_err());
        git::ensure_worktree(&paths, &repo, &worktree, &second)?;
        let second_key = validation::job_dispatcher_key(&repaired)?;
        let second_command = validation::job_dispatcher_spec(&paths, &worktree, &repaired)?;
        assert_ne!(first_key, second_key);
        let result = crate::broker::run(
            &paths,
            &second_key,
            crate::broker::ResourceClass::Supervisor,
            &second_command,
        )?;
        assert_eq!(result.exit_code, Some(9));
        assert!(validation::require_job_dispatcher(&paths, &worktree, &repaired).is_err());
        validation::require_job_dispatcher(&paths, &worktree, &job)?;
        let mut first_environment = first_command.env;
        let mut second_environment = second_command.env;
        first_environment.remove("TELETE_CONTEXT");
        second_environment.remove("TELETE_CONTEXT");
        assert_eq!(first_environment, second_environment);
        Ok(())
    }

    #[test]
    fn cell_commands_use_frozen_host_policy_and_exact_preparation_handoff() -> Result<()> {
        let (_temporary, paths, _repo, base) = fixture()?;
        let mut job = job(&paths, &base, "frozen-cell-commands")?;
        job.base = Some(base.clone());
        job.candidate = Some(base.clone());
        let host = paths.root.join("retained-host-telete");
        let environment = BTreeMap::from([("FROZEN_VALUE".into(), "retained".into())]);
        job.policy.commands = Some(CommandPolicy {
            check_script: "pipeline/cell/check.sh".into(),
            host_program: host.clone(),
            environment: environment.clone(),
        });
        let preparation = cell_pipeline::preparation_command(&paths, &job)?;
        let prepared = Preparation {
            schema: 1,
            source: base.clone(),
            products: vec!["alpha".into()],
            signing_policy: job.policy.signing()?.clone(),
            candidates: BTreeMap::from([(
                "alpha".into(),
                deployment::Candidate {
                    schema: 1,
                    candidate_id: "retained-candidate".into(),
                    source_commit: base.clone(),
                    source_key: base.to_string(),
                    product: "alpha".into(),
                    candidate_dir: paths.root.join("staged/alpha"),
                    binaries: BTreeMap::new(),
                    signing_policy: job.policy.signing()?.clone(),
                },
            )]),
            release_check: true,
        };
        let request = DeploymentRequest {
            id: "retained-deployment".into(),
            source: base,
            products: prepared.products.clone(),
            prepared,
        };
        job.deployment_request = Some(request.clone());
        let deployment = cell_pipeline::deployment_command(&paths, &job)?;
        for command in [&preparation, &deployment] {
            assert_eq!(command.program, host);
            assert_eq!(command.cwd, paths.job(&job.id.0).join("worktree"));
            assert_eq!(command.timeout_seconds, None);
            assert!(!command.confined);
            let context = command.context.as_ref().context("command context")?;
            let mut expected = environment.clone();
            expected.insert("TELETE_CONTEXT".into(), context.path.display().to_string());
            assert_eq!(command.env, expected);
            assert_eq!(context.value["job_id"], json!(job.id));
            assert_eq!(context.value["source"]["candidate"], json!(job.candidate));
        }
        let context = deployment.context.as_ref().context("deployment context")?;
        assert_eq!(context.value["inputs"], serde_json::to_value(request)?);
        assert_eq!(context.value["stage"], "deploy");
        assert_eq!(
            preparation
                .context
                .as_ref()
                .context("preparation context")?
                .value["stage"],
            "prepare"
        );
        Ok(())
    }

    #[test]
    fn frozen_invocation_joins_exact_completion_without_replaying() -> Result<()> {
        let (_temporary, paths, _repo, base) = fixture()?;
        let mut job = job(&paths, &base, "frozen-invocation")?;
        job.base = Some(base.clone());
        job.candidate = Some(base);
        job.policy.commands = Some(CommandPolicy::capture(&paths)?);
        let marker = paths.root.join("invocation-marker");
        let command = crate::invocation::bind(
            &paths,
            &job,
            "retained",
            "check",
            &json!({}),
            &json!({"marker": marker}),
            crate::process::CommandSpec {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "printf x >> \"$1\"; printf complete".into(),
                    "sh".into(),
                    marker.display().to_string(),
                ],
                cwd: paths.root.clone(),
                env: BTreeMap::new(),
                timeout_seconds: None,
                stdin: None,
                confined: false,
                context: None,
            },
        )?;
        let first = crate::invocation::run(&paths, &job, "retained", &command)?;
        assert!(first.success());
        let joined = crate::invocation::run(&paths, &job, "retained", &command)?;
        assert_eq!(serde_json::to_value(first)?, serde_json::to_value(joined)?);
        assert_eq!(fs::read_to_string(&marker)?, "x");
        let mut changed = command.clone();
        changed.args.push("changed".into());
        assert!(crate::invocation::run(&paths, &job, "retained", &changed).is_err());
        changed = command.clone();
        changed.context.as_mut().context("command context")?.value["stage"] = json!("deploy");
        assert!(crate::invocation::run(&paths, &job, "retained", &changed).is_err());
        let directory = crate::invocation::directory(&paths, &job, "retained")?;
        fs::remove_file(directory.join("result.json"))?;
        assert!(crate::invocation::run(&paths, &job, "retained", &command).is_err());
        assert_eq!(fs::read_to_string(marker)?, "x");
        Ok(())
    }

    #[tokio::test]
    async fn validation_recovery_requires_successful_wrapper_and_aggregate_report() -> Result<()> {
        for exit_code in [None, Some(1), Some(0)] {
            let (_temporary, paths, repo, base) = fixture()?;
            let worker = worker(&paths)?;
            let mut blocked = job(&paths, &base, "check-wrapper")?;
            blocked.base = Some(base.clone());
            blocked.candidate = Some(base.clone());
            blocked.phase = Phase::Blocked;
            blocked.stopped_phase = Some(Phase::Checking);
            blocked.outcome = Some(Phase::Failed);
            blocked.unresolved = true;
            let worktree = worker.worktree(&blocked);
            git::ensure_worktree(&paths, &repo, &worktree, &base)?;
            let receipt = worker.directory(&blocked)?.join("validation-0.json");
            blocked.operation = Some(Operation {
                phase: Phase::Checking,
                name: "validation-0".into(),
                candidate: base.clone(),
                result: receipt.clone(),
                started: 1,
            });
            let mut passed = report(&base, &base, &[]);
            for name in ["structure", "recognition", "cargo.metadata"] {
                passed.required_gates.push(name.into());
                passed.gates.push(crate::validation::GateReport {
                    name: name.into(),
                    class: crate::model::ResourceClass::Light,
                    command: vec!["fixture".into()],
                    exit_code: Some(0),
                    timed_out: false,
                    diagnostics: String::new(),
                });
            }
            store::atomic_json(
                &validation::receipt_path(&paths, &base, &base, false, true),
                &passed,
            )?;
            worker.store.insert(&blocked)?;
            retain_check_dispatcher(&paths, &worktree, &base, false, exit_code)?;
            drop(worker);
            let recovered = recover(&paths, &blocked.id.0).await;
            if exit_code == Some(0) {
                assert_eq!(recovered?["phase"], "preparing");
                assert!(receipt.is_file());
            } else {
                assert!(recovered.is_err());
                assert!(!receipt.exists());
                assert_eq!(
                    Store::open(&paths.root, false)?.job(&blocked.id.0)?.phase,
                    Phase::Blocked
                );
            }
            assert_eq!(git::ref_value(&paths, &repo, git::ACCEPTED)?, Some(base));
        }
        Ok(())
    }

    #[test]
    fn autofix_before_requested_tests_advances_but_cannot_pass() -> Result<()> {
        let (_temporary, paths, repo, base) = fixture()?;
        let worker = worker(&paths)?;
        for (id, state) in [
            ("autofix-before-tests", ValidationState::Autofix),
            ("pass-without-tests", ValidationState::Passed),
        ] {
            let mut job = job(&paths, &base, id)?;
            job.base = Some(base.clone());
            job.candidate = Some(base.clone());
            job.phase = Phase::Checking;
            job.options.run_tests = true;
            let directory = worker.directory(&job)?;
            git::ensure_worktree(&paths, &repo, &worker.worktree(&job), &base)?;
            let receipt = directory.join("validation-0.json");
            job.operation = Some(Operation {
                phase: Phase::Checking,
                name: "validation-0".into(),
                candidate: base.clone(),
                result: receipt.clone(),
                started: 1,
            });
            let mut report = report(&base, &base, &["alpha"]);
            report.state = state.clone();
            let raw = "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-base\n+fixed\n";
            if state == ValidationState::Autofix {
                let patch = directory.join("autofix.patch");
                store::atomic_bytes(&patch, raw.as_bytes())?;
                report.autofix_patch = Some(patch);
            }
            store::atomic_json(&receipt, &report)?;
            retain_check_dispatcher(&paths, &worker.worktree(&job), &base, true, Some(0))?;
            let mut job = worker.store.insert(&job)?;

            if state == ValidationState::Autofix {
                worker.checking(&mut job)?;
                let saved = worker.store.job(id)?;
                assert_eq!(saved.phase, Phase::Autofixing);
                assert!(saved.operation.is_none());
                assert_eq!(saved.validations.len(), 1);
                assert!(!saved.validations[0].report.tests_run);
                assert_eq!(saved.validations[0].patch_content.as_deref(), Some(raw));
                assert_eq!(saved.candidate, Some(base.clone()));
            } else {
                let error = match worker.checking(&mut job) {
                    Ok(()) => bail!("tests are required"),
                    Err(error) => error,
                };
                assert!(error.to_string().contains("frozen tests"));
                let saved = worker.store.job(id)?;
                assert_eq!(saved.phase, Phase::Checking);
                assert!(saved.operation.is_some());
                assert!(saved.validations.is_empty());
            }
        }
        assert_eq!(git::ref_value(&paths, &repo, git::ACCEPTED)?, Some(base));
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn blocked_autofix_stop_requires_exact_completion_and_preserves_job() -> Result<()> {
        let (_temporary, paths, repo, base) = fixture()?;
        let store = Store::open(&paths.root, false)?;
        let mut blocked = job(&paths, &base, "stop-autofix")?;
        blocked.base = Some(base.clone());
        blocked.candidate = Some(base.clone());
        blocked.phase = Phase::Blocked;
        blocked.stopped_phase = Some(Phase::Checking);
        blocked.outcome = Some(Phase::Failed);
        blocked.outcome_message = Some("retained validation policy failure".into());
        blocked.last_error = blocked.outcome_message.clone();
        blocked.unresolved = true;
        blocked.cancel_requested = true;
        blocked.options.run_tests = true;
        blocked.options.notify = true;
        blocked.notification = Some(Notification {
            message: email::api::Message {
                subject: "retained failure".into(),
                body: "retained body".into(),
                idempotency_key: Some("retained-failure-key".into()),
            },
            accepted: true,
            uncertain: false,
            attempts: Vec::new(),
            created: 1_700_000_003,
        });
        let directory = paths.job(&blocked.id.0);
        let worktree = directory.join("worktree");
        git::ensure_worktree(&paths, &repo, &worktree, &base)?;
        git::advance(
            &paths,
            &repo,
            &git::private_ref(&blocked.id.0, "candidate")?,
            None,
            &base,
        )?;
        let receipt = directory.join("validation-0.json");
        blocked.operation = Some(Operation {
            phase: Phase::Checking,
            candidate: base.clone(),
            name: "validation-0".into(),
            result: receipt.clone(),
            started: 1_700_000_002,
        });
        let mut blocked = store.insert(&blocked)?;
        assert!(require_service_stoppable(&paths).is_err());
        let output = validation::receipt_path(&paths, &base, &base, true, true);
        let mut report = report(&base, &base, &["alpha"]);
        report.state = ValidationState::Autofix;
        let patch = output
            .parent()
            .context("validation receipt directory")?
            .join("autofix.patch");
        store::atomic_bytes(&patch, b"retained patch")?;
        report.autofix_patch = Some(patch);
        store::atomic_json(&receipt, &report)?;
        store::atomic_json(&output, &report)?;
        assert!(require_service_stoppable(&paths).is_err());

        let mut env = paths.environment();
        env.insert("CARGO_BUILD_JOBS".into(), "2".into());
        let dispatcher = crate::process::CommandSpec {
            program: paths
                .targets()
                .join("candidate-validator")
                .join(base.to_string())
                .join("debug/telete"),
            args: vec![
                "--state".into(),
                paths.root.display().to_string(),
                "internal-validate".into(),
                "--repo".into(),
                worktree.display().to_string(),
                "--base".into(),
                base.to_string(),
                "--candidate".into(),
                base.to_string(),
                "--receipt".into(),
                output.display().to_string(),
                "--run-tests".into(),
                "--defer-release-builds".into(),
            ],
            cwd: worktree.clone(),
            env,
            timeout_seconds: None,
            stdin: None,
            confined: false,
            context: None,
        };
        let key = format!("validation/{base}/{base}/dispatcher-unbounded-tests-1-defer-1");
        let record = paths
            .root
            .join("broker")
            .join(format!("{}.json", key.replace('/', "_")));
        let process = record.with_extension("process");
        let retain = |exit_code| -> Result<()> {
            let result = crate::process::ProcessResult {
                start_error: None,
                signal: None,
                output_error: None,
                exit_code: Some(exit_code),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            };
            store::atomic_json(
                &record,
                &json!({"schema":1,"key":key,"class":"supervisor","command":dispatcher,
                    "state":"finished","owner_pid":1,"result":result}),
            )?;
            store::atomic_json(&process.join("request.json"), &dispatcher)?;
            store::atomic_json(
                &process.join("result.json"),
                &json!({"schema":1,"request":dispatcher,"result":result}),
            )
        };
        retain(1)?;
        assert!(require_service_stoppable(&paths).is_err());
        retain(0)?;
        report.products = vec!["beta".into()];
        store::atomic_json(&output, &report)?;
        assert!(require_service_stoppable(&paths).is_err());
        report.products = vec!["alpha".into()];
        store::atomic_json(&output, &report)?;
        blocked.notification.as_mut().context("notice")?.uncertain = true;
        store.save(&mut blocked)?;
        assert!(require_service_stoppable(&paths).is_err());
        blocked.notification.as_mut().context("notice")?.uncertain = false;
        blocked.preparation_generation = 1;
        store.save(&mut blocked)?;
        assert!(require_service_stoppable(&paths).is_err());
        blocked.preparation_generation = 0;
        store.save(&mut blocked)?;
        fs::write(worktree.join("file.txt"), "changed source\n")?;
        assert!(require_service_stoppable(&paths).is_err());
        fs::write(worktree.join("file.txt"), "base\n")?;
        store.set("paused", &false)?;
        assert!(require_service_stoppable(&paths).is_err());
        store.set("paused", &true)?;

        let before = serde_json::to_value(store.job(&blocked.id.0)?)?;
        let receipt_before = fs::read(&receipt)?;
        let dispatcher_before = fs::read(process.join("result.json"))?;
        let _resources = crate::broker::settled_guards(&paths)?;
        let _deployment = store::lock(&paths.root.join("deployments/deployment.lock"), false)?;
        let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
        require_service_stoppable(&paths)?;
        assert_eq!(serde_json::to_value(store.job(&blocked.id.0)?)?, before);
        assert_eq!(fs::read(&receipt)?, receipt_before);
        assert_eq!(fs::read(&output)?, receipt_before);
        assert_eq!(fs::read(process.join("result.json"))?, dispatcher_before);
        assert_eq!(git::ref_value(&paths, &repo, git::ACCEPTED)?, Some(base));
        assert!(require_quiescent(&paths).is_err());
        Ok(())
    }

    fn repair(job: &Job, paths: &Paths, number: u64, raw: &str) -> Result<RepairAttempt> {
        let request = providers::request(
            &format!("telete-{}-repair-{number}", job.id),
            &job.id.0,
            &paths.job(&job.id.0).join("worktree"),
            &json!({}),
            job.policy.prompts.as_ref().context("fixture prompts")?,
            &job.policy.primary_model,
            job.policy.primary_reasoning,
            600,
        )?;
        let patch = paths.job(&job.id.0).join(format!("repair-{number}.patch"));
        store::atomic_bytes(&patch, raw.as_bytes())?;
        Ok(RepairAttempt {
            number,
            request,
            parent: job.candidate.clone().context("fixture candidate")?,
            stamp: 1_700_000_001,
            state: "completed".into(),
            patch: Some(patch),
            patch_content: Some(raw.into()),
            candidate: None,
            rejection: None,
            transport_failures: 0,
            terminal: None,
        })
    }

    #[test]
    fn store_preserves_cancellation_and_rejects_stale_history() -> Result<()> {
        let (_temporary, paths, _repo, base) = fixture()?;
        let store = Store::open(&paths.root, false)?;
        let queued = job(&paths, &base, "cas-fixture")?;
        let mut first = store.insert(&queued)?;
        let mut stale = first.clone();
        store.cancel(&first.id.0)?;
        first.waiting_reason = Some("retained".into());
        store.save(&mut first)?;
        assert!(first.cancel_requested);
        stale.waiting_reason = Some("must not overwrite".into());
        assert!(store.save(&mut stale).is_err());
        assert_eq!(
            store.job(&first.id.0)?.waiting_reason.as_deref(),
            Some("retained")
        );
        assert!(store.insert(&queued).is_err());
        assert_eq!(
            store
                .by_request(&queued.request_id)?
                .context("saved request")?
                .id,
            queued.id
        );
        Ok(())
    }

    #[test]
    fn fresh_integration_creates_private_job_directory_before_git() -> Result<()> {
        let (_temporary, paths, repo, base) = fixture()?;
        fs::write(repo.join("file.txt"), "submitted\n")?;
        git::value(&paths, &repo, &["add", "file.txt"])?;
        let tree = git::value(&paths, &repo, &["write-tree"])?;
        let input = git::commit_tree(
            &paths,
            &repo,
            &tree,
            std::slice::from_ref(&base),
            "fixture",
            1_700_000_001,
            "Fixture submission",
        )?;
        git::value(&paths, &repo, &["reset", "--hard", &input.0])?;
        let worker = worker(&paths)?;
        let mut job = job(&paths, &base, "fresh-integration")?;
        job.input = input.clone();
        job.base = Some(base.clone());
        job.phase = Phase::Integrating;
        let mut job = worker.store.insert(&job)?;
        let directory = paths.job(&job.id.0);
        assert!(!directory.exists());

        worker.integrating(&mut job)?;

        assert_eq!(job.phase, Phase::Checking);
        assert_eq!(
            fs::metadata(&directory)?.permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join("merge.log"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let candidate = job.candidate.as_ref().context("integrated candidate")?;
        assert_eq!(
            git::commit(&paths, &worker.worktree(&job), "HEAD")?,
            *candidate
        );
        assert_eq!(
            git::ref_value(&paths, &repo, &git::private_ref(&job.id.0, "candidate")?)?,
            Some(candidate.clone())
        );
        assert_eq!(
            fs::read_to_string(worker.worktree(&job).join("file.txt"))?,
            "submitted\n"
        );
        assert_eq!(git::ref_value(&paths, &repo, git::ACCEPTED)?, Some(base));
        assert_eq!(git::commit(&paths, &repo, "HEAD")?, input);
        Ok(())
    }

    #[test]
    fn explicit_deployment_rechecks_source_already_in_accepted_history() -> Result<()> {
        let (_temporary, paths, repo, base) = fixture()?;
        let worker = worker(&paths)?;
        let mut ordinary = job(&paths, &base, "already-included")?;
        ordinary.base = Some(base.clone());
        ordinary.phase = Phase::Integrating;
        let mut ordinary = worker.store.insert(&ordinary)?;
        worker.integrating(&mut ordinary)?;
        assert_eq!(ordinary.phase, Phase::AlreadyIncluded);

        let mut requested = job(&paths, &base, "explicit-redeployment")?;
        requested.base = Some(base.clone());
        requested.phase = Phase::Integrating;
        requested.options.deploy = vec!["alpha".into()];
        let mut requested = worker.store.insert(&requested)?;
        worker.integrating(&mut requested)?;
        assert_eq!(requested.phase, Phase::Checking);
        assert!(requested.validations.is_empty());
        assert!(requested.preparation.is_none());
        assert!(!requested.accepted);
        let candidate = requested.candidate.as_ref().context("candidate")?;
        assert_ne!(candidate, &base);
        assert_eq!(
            git::value(
                &paths,
                &repo,
                &["rev-parse", &format!("{candidate}^{{tree}}")]
            )?,
            git::value(&paths, &repo, &["rev-parse", &format!("{base}^{{tree}}")])?
        );
        assert_eq!(git::ref_value(&paths, &repo, git::ACCEPTED)?, Some(base));
        Ok(())
    }

    #[test]
    fn settings_are_frozen_and_invalid_submission_choices_are_rejected() -> Result<()> {
        let (_temporary, paths, repo, base) = fixture()?;
        let file = paths.root.join("settings-input.json");
        fs::write(&file, br#"{"alpha":{"enabled":true}}"#)?;
        let settings = read_settings(Some(&file))?;
        fs::write(&file, br#"{"alpha":{"enabled":false}}"#)?;
        assert_eq!(settings["alpha"], json!({"enabled":true}));
        assert!(read_settings(Some(Path::new("relative.json"))).is_err());
        fs::write(&file, b"[]")?;
        assert!(read_settings(Some(&file)).is_err());
        for (deploy, settings) in [
            (Vec::new(), settings.clone()),
            (vec!["beta".into()], settings),
            (
                vec!["alpha".into()],
                BTreeMap::from([("alpha".into(), json!(true))]),
            ),
            (
                vec!["decisions".into()],
                BTreeMap::from([("decisions".into(), json!({}))]),
            ),
        ] {
            assert!(
                submit(
                    &paths,
                    &repo,
                    &base.0,
                    SubmitOptions {
                        deploy,
                        settings,
                        ..SubmitOptions::default()
                    }
                )
                .is_err()
            );
        }
        let old = serde_json::to_value(SubmitOptions::default())?;
        assert!(old.get("settings").is_none());
        assert!(
            serde_json::from_value::<SubmitOptions>(old)?
                .settings
                .is_empty()
        );

        let mut frozen = job(&paths, &base, "settings-frozen")?;
        frozen.options.request_id = Some(frozen.request_id.clone());
        frozen.options.deploy = vec!["alpha".into()];
        frozen.options.settings = BTreeMap::from([("alpha".into(), json!({"enabled":true}))]);
        let store = Store::open(&paths.root, false)?;
        let frozen = store.insert(&frozen)?;
        let joined = submit(&paths, &repo, &base.0, frozen.options.clone())?;
        assert_eq!(joined["id"], serde_json::to_value(&frozen.id)?);
        let mut changed = frozen.options.clone();
        changed
            .settings
            .insert("alpha".into(), json!({"enabled":false}));
        assert!(submit(&paths, &repo, &base.0, changed).is_err());
        assert_eq!(store.job(&frozen.id.0)?.options, frozen.options);
        Ok(())
    }

    #[test]
    fn preparation_acknowledgement_abandons_without_replay_or_new_outcome() -> Result<()> {
        let (_temporary, paths, repo, base) = fixture()?;
        let store = Store::open(&paths.root, false)?;
        let mut blocked = job(&paths, &base, "abandon-preparation")?;
        blocked.base = Some(base.clone());
        blocked.candidate = Some(base.clone());
        blocked.phase = Phase::Blocked;
        blocked.stopped_phase = Some(Phase::Preparing);
        blocked.outcome = Some(Phase::Failed);
        blocked.outcome_message = Some("retained staging failure".into());
        blocked.last_error = blocked.outcome_message.clone();
        blocked.unresolved = true;
        blocked.operation = Some(Operation {
            phase: Phase::Preparing,
            candidate: base.clone(),
            name: "production-0".into(),
            result: paths.job(&blocked.id.0).join("production-0.json"),
            started: 1_700_000_002,
        });
        blocked.options.notify = true;
        blocked.notification = Some(Notification {
            message: email::api::Message {
                subject: "retained failure".into(),
                body: "retained body".into(),
                idempotency_key: Some("retained-key".into()),
            },
            accepted: true,
            uncertain: false,
            attempts: Vec::new(),
            created: 1_700_000_003,
        });
        let blocked = store.insert(&blocked)?;
        let directory = paths.job(&blocked.id.0);
        git::ensure_worktree(&paths, &repo, &directory.join("worktree"), &base)?;
        store.set("paused", &false)?;
        assert!(acknowledge_preparation(&paths, &blocked.id.0).is_err());
        store.set("paused", &true)?;
        let active_child = store::lock(&paths.root.join("worker.lock"), false)?;
        assert!(acknowledge_preparation(&paths, &blocked.id.0).is_err());
        drop(active_child);

        acknowledge_preparation(&paths, &blocked.id.0)?;
        let settled = store.job(&blocked.id.0)?;
        assert_eq!(settled.phase, Phase::Failed);
        assert!(!settled.unresolved);
        assert!(settled.operation.is_none());
        assert_eq!(settled.last_error, blocked.last_error);
        assert_eq!(settled.outcome_message, blocked.outcome_message);
        assert_eq!(settled.outcome_generation, blocked.outcome_generation);
        assert_eq!(
            serde_json::to_value(settled.notification)?,
            serde_json::to_value(blocked.notification)?
        );
        assert_eq!(settled.cleanup.state, "removed");
        assert!(directory.join("production-0.acknowledged.json").is_file());
        assert!(!directory.join("production-0.json").exists());
        assert_eq!(git::ref_value(&paths, &repo, git::ACCEPTED)?, Some(base));
        acknowledge_preparation(&paths, &blocked.id.0)?;
        assert_eq!(store.job(&blocked.id.0)?.revision, settled.revision);
        Ok(())
    }

    #[test]
    fn blocked_validation_without_missing_intent_keeps_ordinary_cancellation() -> Result<()> {
        let (_temporary, paths, _repo, base) = fixture()?;
        let store = Store::open(&paths.root, false)?;
        for (id, receipt) in [("no-operation", false), ("completed-report", true)] {
            let mut blocked = job(&paths, &base, id)?;
            blocked.base = Some(base.clone());
            blocked.candidate = Some(base.clone());
            blocked.phase = Phase::Blocked;
            blocked.stopped_phase = Some(Phase::Checking);
            blocked.outcome = Some(Phase::Failed);
            blocked.unresolved = true;
            blocked.options.notify = true;
            blocked.notification = Some(Notification {
                message: email::api::Message {
                    subject: "retained failure".into(),
                    body: "retained body".into(),
                    idempotency_key: Some(format!("retained-{id}")),
                },
                accepted: true,
                uncertain: false,
                attempts: Vec::new(),
                created: 1,
            });
            if receipt {
                let result = paths.job(id).join("validation-0.json");
                store::atomic_json(&result, &report(&base, &base, &[]))?;
                blocked.operation = Some(Operation {
                    phase: Phase::Checking,
                    name: "validation-0".into(),
                    candidate: base.clone(),
                    result,
                    started: 1,
                });
            }
            store.insert(&blocked)?;
            cancel(&paths, id)?;
            let retained = store.job(id)?;
            assert!(retained.cancel_requested);
            assert_eq!(retained.phase, Phase::Blocked);
            assert!(retained.unresolved);
            assert_eq!(
                serde_json::to_value(retained.operation)?,
                serde_json::to_value(blocked.operation)?
            );
        }
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn timed_out_validation_cancellation_requires_settled_evidence_and_preserves_failure()
    -> Result<()> {
        let (_temporary, paths, repo, base) = fixture()?;
        let store = Store::open(&paths.root, false)?;
        let mut blocked = job(&paths, &base, "cancel-validation")?;
        blocked.base = Some(base.clone());
        blocked.candidate = Some(base.clone());
        blocked.phase = Phase::Blocked;
        blocked.stopped_phase = Some(Phase::Checking);
        blocked.outcome = Some(Phase::Failed);
        blocked.outcome_message = Some("retained validator timeout".into());
        blocked.last_error = blocked.outcome_message.clone();
        blocked.unresolved = true;
        blocked.options.run_tests = true;
        blocked.options.notify = true;
        blocked.notification = Some(Notification {
            message: email::api::Message {
                subject: "retained failure".into(),
                body: "retained body".into(),
                idempotency_key: Some("retained-failure-key".into()),
            },
            accepted: true,
            uncertain: false,
            attempts: Vec::new(),
            created: 1_700_000_003,
        });
        let directory = paths.job(&blocked.id.0);
        let worktree = directory.join("worktree");
        git::ensure_worktree(&paths, &repo, &worktree, &base)?;
        git::advance(
            &paths,
            &repo,
            &git::private_ref(&blocked.id.0, "candidate")?,
            None,
            &base,
        )?;
        blocked.operation = Some(Operation {
            phase: Phase::Checking,
            candidate: base.clone(),
            name: "validation-0".into(),
            result: directory.join("validation-0.json"),
            started: 1_700_000_002,
        });
        let blocked = store.insert(&blocked)?;
        let output = validation::receipt_path(&paths, &base, &base, true, true);
        let mut env = paths.environment();
        env.insert("CARGO_BUILD_JOBS".into(), "2".into());
        let dispatcher = crate::process::CommandSpec {
            program: paths
                .targets()
                .join("candidate-validator")
                .join(base.to_string())
                .join("debug/telete"),
            args: vec![
                "--state".into(),
                paths.root.display().to_string(),
                "internal-validate".into(),
                "--repo".into(),
                worktree.display().to_string(),
                "--base".into(),
                base.to_string(),
                "--candidate".into(),
                base.to_string(),
                "--receipt".into(),
                output.display().to_string(),
                "--run-tests".into(),
                "--defer-release-builds".into(),
            ],
            cwd: worktree.clone(),
            env,
            timeout_seconds: Some(1800),
            stdin: None,
            confined: false,
            context: None,
        };
        let retain = |key: &str,
                      command: &crate::process::CommandSpec,
                      class: crate::broker::ResourceClass,
                      result: Option<crate::process::ProcessResult>|
         -> Result<PathBuf> {
            let stem = key.replace('/', "_");
            let record = paths.root.join("broker").join(format!("{stem}.json"));
            store::atomic_json(
                &record,
                &json!({"schema":1,"key":key,"class":class,"command":command,
                    "state":if result.is_some(){"finished"}else{"running"},
                    "owner_pid":1,"result":result}),
            )?;
            let process = record.with_extension("process");
            store::atomic_json(&process.join("request.json"), command)?;
            if let Some(result) = result {
                store::atomic_json(
                    &process.join("result.json"),
                    &json!({"schema":1,"request":command,"result":result}),
                )?;
            }
            Ok(process)
        };
        let key = format!("validation/{base}/{base}/dispatcher-tests-1-defer-1");
        let complete = crate::process::ProcessResult {
            start_error: None,
            signal: None,
            output_error: None,
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        };
        retain(
            &key,
            &dispatcher,
            crate::broker::ResourceClass::Supervisor,
            Some(complete.clone()),
        )?;
        assert!(cancel(&paths, &blocked.id.0).is_err());
        assert!(!store.job(&blocked.id.0)?.cancel_requested);
        let timeout = crate::process::ProcessResult {
            start_error: None,
            signal: None,
            output_error: None,
            exit_code: None,
            timed_out: true,
            ..complete.clone()
        };
        let dispatcher_process = retain(
            &key,
            &dispatcher,
            crate::broker::ResourceClass::Supervisor,
            Some(timeout),
        )?;
        let retained_dispatcher = fs::read(dispatcher_process.join("result.json"))?;
        let child_key = format!("validation/{base}/{base}/rust.tests");
        let child = retain(
            &child_key,
            &dispatcher,
            crate::broker::ResourceClass::Heavy,
            None,
        )?;
        assert!(cancel(&paths, &blocked.id.0).is_err());
        store::atomic_json(
            &child.join("result.json"),
            &json!({"schema":1,"request":dispatcher,"result":complete}),
        )?;
        let resource = store::lock(&paths.root.join("broker/slot-heavy-0.lock"), false)?;
        assert!(cancel(&paths, &blocked.id.0).is_err());
        drop(resource);
        let deployment = store::lock(&paths.root.join("deployments/deployment.lock"), false)?;
        assert!(cancel(&paths, &blocked.id.0).is_err());
        drop(deployment);
        store.set("paused", &false)?;
        assert!(cancel(&paths, &blocked.id.0).is_err());
        store.set("paused", &true)?;
        fs::write(worktree.join("file.txt"), "changed source\n")?;
        assert!(cancel(&paths, &blocked.id.0).is_err());
        fs::write(worktree.join("file.txt"), "base\n")?;
        // A blocked worker can remain loaded; settled child ownership is required.
        let blocked_worker = store::lock(&paths.root.join("worker.lock"), false)?;
        cancel(&paths, &blocked.id.0)?;
        drop(blocked_worker);
        let settled = store.job(&blocked.id.0)?;
        assert_eq!(settled.phase, Phase::Cancelled);
        assert!(settled.cancel_requested);
        assert!(!settled.unresolved);
        assert!(settled.operation.is_none());
        assert_eq!(settled.outcome, blocked.outcome);
        assert_eq!(settled.outcome_message, blocked.outcome_message);
        assert_eq!(settled.outcome_generation, blocked.outcome_generation);
        assert_eq!(settled.last_error, blocked.last_error);
        assert_eq!(
            serde_json::to_value(&settled.notification)?,
            serde_json::to_value(&blocked.notification)?
        );
        let record: ValidationAbandonment =
            serde_json::from_slice(&fs::read(directory.join("validation-0.abandoned.json"))?)?;
        assert_eq!(
            serde_json::to_value(record.operation)?,
            serde_json::to_value(&blocked.operation)?
        );
        assert_eq!(
            fs::read(dispatcher_process.join("result.json"))?,
            retained_dispatcher
        );
        assert!(!output.exists());
        assert!(!directory.join("validation-0.json").exists());
        assert_eq!(git::ref_value(&paths, &repo, git::ACCEPTED)?, Some(base));
        assert!(require_quiescent(&paths).is_ok());
        cancel(&paths, &blocked.id.0)?;
        assert_eq!(store.job(&blocked.id.0)?.revision, settled.revision);
        Ok(())
    }

    #[test]
    fn production_and_deployment_scope_preserve_explicit_intent() -> Result<()> {
        let (_temporary, paths, repo, base) = fixture()?;
        let worker = worker(&paths)?;
        let mut job = job(&paths, &base, "scope-fixture")?;
        job.base = Some(base.clone());
        job.candidate = Some(base.clone());
        git::ensure_worktree(&paths, &repo, &worker.worktree(&job), &base)?;
        job.validations.push(Validation {
            report: report(&base, &base, &["alpha", "telete"]),
            diagnostics: paths.job(&job.id.0).join("diagnostics"),
            stamp: 1,
            patch_content: None,
            fixed_candidate: None,
        });
        job.options.deploy = vec!["beta".into()];
        assert_eq!(
            worker.production_products(&job)?,
            vec!["alpha", "beta", "telete"]
        );
        assert_eq!(worker.deployment_products(&job)?, vec!["beta"]);
        job.options.no_deploy = true;
        assert_eq!(
            worker.production_products(&job)?,
            vec!["alpha", "beta", "telete"]
        );
        assert!(worker.deployment_products(&job)?.is_empty());
        job.options.no_deploy = false;
        job.options.deploy.clear();
        assert_eq!(worker.deployment_products(&job)?, vec!["alpha"]);
        job.validations
            .last_mut()
            .context("validation")?
            .report
            .products = vec!["decisions".into()];
        assert_eq!(worker.production_products(&job)?, vec!["krisis"]);
        assert_eq!(worker.deployment_products(&job)?, vec!["krisis"]);
        Ok(())
    }

    #[test]
    fn accepted_git_patch_refunds_only_recorded_private_candidate() -> Result<()> {
        let (_temporary, paths, repo, base) = fixture()?;
        let worker = worker(&paths)?;
        let mut job = job(&paths, &base, "refund-fixture")?;
        job.base = Some(base.clone());
        job.candidate = Some(base.clone());
        job.phase = Phase::Applying;
        store::private_directory(&paths.job(&job.id.0))?;
        git::ensure_worktree(&paths, &repo, &worker.worktree(&job), &base)?;
        git::advance(
            &paths,
            &repo,
            &git::private_ref(&job.id.0, "candidate")?,
            None,
            &base,
        )?;
        let raw = "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1,99 +1,99 @@\n-base\n+patched";
        job.attempts.push(repair(&job, &paths, 1, raw)?);
        let mut job = worker.store.insert(&job)?;
        assert_eq!(job.charged_attempts(), 1);
        worker.applying(&mut job)?;
        assert_eq!(job.charged_attempts(), 0);
        assert_eq!(job.phase, Phase::Checking);
        assert_eq!(job.budget()["invocations"], 1);
        assert_eq!(job.budget()["refunded"], 1);
        assert_eq!(
            fs::read_to_string(worker.worktree(&job).join("file.txt"))?,
            "patched\n"
        );
        assert_eq!(
            git::ref_value(&paths, &repo, git::ACCEPTED)?,
            Some(base.clone())
        );
        assert_eq!(git::commit(&paths, &repo, "HEAD")?, base);
        job.attempts
            .push(repair(&job, &paths, 2, "this is not a Git patch")?);
        job.phase = Phase::Applying;
        worker.store.save(&mut job)?;
        worker.applying(&mut job)?;
        assert_eq!(job.charged_attempts(), 1);
        assert_eq!(job.phase, Phase::RepairPrepare);
        assert!(job.attempts[1].rejection.is_some());
        assert_eq!(
            job.attempts[1].request.id.as_str(),
            "telete-refund-fixture-repair-2"
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_validation_completion_blocks_and_cannot_be_replayed() -> Result<()> {
        let (_temporary, paths, _repo, base) = fixture()?;
        let worker = worker(&paths)?;
        let mut job = job(&paths, &base, "lost-validator")?;
        job.base = Some(base.clone());
        job.candidate = Some(base.clone());
        job.phase = Phase::Checking;
        let directory = worker.directory(&job)?;
        job.operation = Some(Operation {
            phase: Phase::Checking,
            name: "validation-0".into(),
            candidate: base.clone(),
            result: directory.join("validation-0.json"),
            started: 1,
        });
        worker.store.insert(&job)?;
        drop(worker);
        let result = worker_once(&paths).await?;
        assert_eq!(result["phase"], "blocked");
        assert_eq!(result["unresolved"], true);
        assert!(recover(&paths, "lost-validator").await.is_err());
        assert_eq!(status(&paths, Some("lost-validator"))?["phase"], "blocked");
        Ok(())
    }

    #[test]
    fn cleanup_removes_only_settled_private_worktree_and_registration() -> Result<()> {
        let (_temporary, paths, repo, base) = fixture()?;
        let worker = worker(&paths)?;
        let mut job = job(&paths, &base, "cleanup-fixture")?;
        job.base = Some(base.clone());
        job.candidate = Some(base.clone());
        job.phase = Phase::Checking;
        let mut job = worker.store.insert(&job)?;
        store::private_directory(&paths.job(&job.id.0))?;
        git::ensure_worktree(&paths, &repo, &worker.worktree(&job), &base)?;
        fs::write(worker.worktree(&job).join("private-untracked"), "private")?;
        let registration = git::registration(&paths, &repo, &worker.worktree(&job))?
            .context("linked registration")?;
        worker.finish(&mut job, Phase::Failed, "fixture failure".into(), true)?;
        assert!(worker.worktree(&job).exists());
        assert!(registration.exists());
        job.unresolved = false;
        job.phase = Phase::Cancelled;
        job.outcome = Some(Phase::Cancelled);
        worker.store.save(&mut job)?;
        worker.cleanup(&mut job)?;
        assert_eq!(job.cleanup.state, "removed");
        assert!(!worker.worktree(&job).exists());
        assert!(!registration.exists());
        assert_eq!(git::commit(&paths, &repo, "HEAD")?, base);
        Ok(())
    }

    #[test]
    fn maintenance_requires_paused_and_settled_queue() -> Result<()> {
        let (_temporary, paths, _repo, base) = fixture()?;
        assert!(require_quiescent(&paths).is_ok());
        let store = Store::open(&paths.root, false)?;
        store.set("paused", &false)?;
        assert!(require_quiescent(&paths).is_err());
        store.set("paused", &true)?;
        let queued = job(&paths, &base, "maintenance-fixture")?;
        store.insert(&queued)?;
        assert!(require_quiescent(&paths).is_err());
        drop(store);
        cancel(&paths, &queued.id.0)?;
        assert!(require_quiescent(&paths).is_ok());
        Ok(())
    }

    #[test]
    fn maintenance_drain_keeps_non_model_operations_visible() -> Result<()> {
        let (_temporary, paths, _repo, base) = fixture()?;
        let store = Store::open(&paths.root, false)?;
        let mut active = job(&paths, &base, "drain-fixture")?;
        active.phase = Phase::Preparing;
        active.base = Some(base.clone());
        active.candidate = Some(base);
        active.model_unresolved = false;
        store.insert(&active)?;
        drop(store);
        let held = maintenance_hold(&paths, "fixture-owner")?;
        assert_eq!(held["held"], true);
        assert_eq!(held["drained"], false);
        assert_eq!(held["active_job"], "drain-fixture");
        Ok(())
    }

    #[tokio::test]
    async fn recovery_reads_completed_preparation_without_repeating_it() -> Result<()> {
        let (_temporary, paths, _repo, base) = fixture()?;
        let worker = worker(&paths)?;
        let mut job = job(&paths, &base, "prepared-fixture")?;
        job.base = Some(base.clone());
        job.candidate = Some(base.clone());
        job.phase = Phase::Blocked;
        job.stopped_phase = Some(Phase::Preparing);
        job.unresolved = true;
        job.outcome = Some(Phase::Failed);
        job.validations.push(Validation {
            report: report(&base, &base, &[]),
            diagnostics: paths.job(&job.id.0).join("diagnostics"),
            stamp: 1,
            patch_content: None,
            fixed_candidate: None,
        });
        let directory = worker.directory(&job)?;
        let receipt = directory.join("production-0.json");
        job.operation = Some(Operation {
            phase: Phase::Preparing,
            name: "production-0".into(),
            candidate: base.clone(),
            result: receipt.clone(),
            started: 1,
        });
        let prepared = Preparation {
            schema: 1,
            source: base.clone(),
            products: Vec::new(),
            signing_policy: job.policy.signing()?.clone(),
            candidates: BTreeMap::new(),
            release_check: true,
        };
        store::atomic_json(&directory.join("production-0/result.json"), &prepared)?;
        worker.store.insert(&job)?;
        drop(worker);
        let recovered = recover(&paths, &job.id.0).await?;
        assert_eq!(recovered["phase"], "accepting");
        assert_eq!(recovered["unresolved"], false);
        assert_eq!(recovered["preparation_generation"], 1);
        let retained: PreparationOutcome = serde_json::from_slice(&fs::read(receipt)?)?;
        assert_eq!(
            retained.prepared.context("recovered preparation")?,
            prepared
        );
        assert!(retained.compilation_failure.is_none());
        Ok(())
    }

    #[test]
    fn cell_preparation_command_requires_the_exact_retained_operation() -> Result<()> {
        let (_temporary, paths, _repo, base) = fixture()?;
        let worker = worker(&paths)?;
        let mut job = job(&paths, &base, "cell-preparation-command")?;
        job.base = Some(base.clone());
        job.candidate = Some(base.clone());
        job.phase = Phase::Preparing;
        job.validations.push(Validation {
            report: report(&base, &base, &[]),
            diagnostics: paths.job(&job.id.0).join("diagnostics"),
            stamp: 1,
            patch_content: None,
            fixed_candidate: None,
        });
        let directory = worker.directory(&job)?;
        let receipt = directory.join("production-0.json");
        job.operation = Some(Operation {
            phase: Phase::Preparing,
            name: "production-0".into(),
            candidate: base.clone(),
            result: receipt.clone(),
            started: 1,
        });
        worker.store.insert(&job)?;
        let command = cell_pipeline::preparation_command(&paths, &job)?;
        assert_eq!(command.program, std::env::current_exe()?);
        assert_eq!(command.cwd, worker.worktree(&job));
        assert_eq!(command.timeout_seconds, None);
        assert!(!command.confined);
        assert_eq!(
            command.args,
            vec![
                "--state".to_owned(),
                paths.root.display().to_string(),
                "internal-cell-prepare".to_owned(),
                "--job".to_owned(),
                job.id.to_string(),
                "--generation".to_owned(),
                "0".to_owned(),
                "--candidate".to_owned(),
                base.to_string(),
            ]
        );
        assert!(cell_pipeline::execute_prepare(&paths, &job.id.0, 1, &base).is_err());
        let other = CommitId::new("b".repeat(40))?;
        assert!(cell_pipeline::execute_prepare(&paths, &job.id.0, 0, &other).is_err());
        assert!(!receipt.try_exists()?);
        cell_pipeline::execute_prepare(&paths, &job.id.0, 0, &base)?;
        let bytes = fs::read(&receipt)?;
        let outcome: PreparationOutcome = serde_json::from_slice(&bytes)?;
        assert_eq!(outcome.prepared.context("prepared outcome")?.source, base);
        assert!(outcome.compilation_failure.is_none());
        assert!(cell_pipeline::execute_prepare(&paths, &job.id.0, 0, &base).is_err());
        assert_eq!(fs::read(receipt)?, bytes);
        assert_eq!(worker.store.job(&job.id.0)?.phase, Phase::Preparing);
        assert_eq!(
            git::ref_value(&paths, &worker.config.repository, git::ACCEPTED)?,
            Some(base)
        );
        Ok(())
    }

    #[test]
    fn cell_deployment_command_requires_the_exact_accepted_request() -> Result<()> {
        let (_temporary, paths, _repo, base) = fixture()?;
        let worker = worker(&paths)?;
        let mut job = job(&paths, &base, "cell-deployment-command")?;
        job.base = Some(base.clone());
        job.candidate = Some(base.clone());
        job.phase = Phase::Deploying;
        job.accepted = true;
        let request_id = "cell-deployment-fixture";
        job.deployment_request = Some(DeploymentRequest {
            id: request_id.into(),
            source: base.clone(),
            products: vec!["alpha".into()],
            prepared: Preparation {
                schema: 1,
                source: base.clone(),
                products: vec!["alpha".into()],
                signing_policy: job.policy.signing()?.clone(),
                candidates: BTreeMap::new(),
                release_check: true,
            },
        });
        let mut job = worker.store.insert(&job)?;
        let command = cell_pipeline::deployment_command(&paths, &job)?;
        assert_eq!(command.program, std::env::current_exe()?);
        assert_eq!(command.cwd, worker.worktree(&job));
        assert_eq!(command.timeout_seconds, None);
        assert!(!command.confined);
        assert_eq!(
            command.args,
            vec![
                "--state".to_owned(),
                paths.root.display().to_string(),
                "internal-cell-deploy".to_owned(),
                "--job".to_owned(),
                job.id.to_string(),
                "--request-id".to_owned(),
                request_id.to_owned(),
                "--candidate".to_owned(),
                base.to_string(),
            ]
        );
        assert!(cell_pipeline::execute_deploy(&paths, &job.id.0, "other-request", &base).is_err());
        let other = CommitId::new("b".repeat(40))?;
        assert!(cell_pipeline::execute_deploy(&paths, &job.id.0, request_id, &other).is_err());
        job.accepted = false;
        worker.store.save(&mut job)?;
        assert!(cell_pipeline::execute_deploy(&paths, &job.id.0, request_id, &base).is_err());
        job.accepted = true;
        job.options.no_deploy = true;
        worker.store.save(&mut job)?;
        assert!(cell_pipeline::execute_deploy(&paths, &job.id.0, request_id, &base).is_err());
        assert!(!paths.root.join("deployments/index.json").try_exists()?);
        assert_eq!(worker.store.job(&job.id.0)?.phase, Phase::Deploying);
        Ok(())
    }

    include!("workflow_tests.rs");
    include!("bootstrap_cancel_tests.rs");
    include!("preparation_stop_tests.rs");
}
