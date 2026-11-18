//! Configured project commands, frozen with each submission.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::model::CommitId;
use crate::process::{CommandSpec, ProcessResult};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowCommand {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl WorkflowCommand {
    fn validate(&self, stage: &str) -> Result<()> {
        ensure!(
            !self.program.as_os_str().is_empty(),
            "{stage} program is empty"
        );
        ensure!(
            self.program
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
                && self
                    .program
                    .as_os_str()
                    .as_encoded_bytes()
                    .split(|byte| *byte == b'/')
                    .enumerate()
                    .all(|(index, part)| (index == 0 && self.program.is_absolute())
                        || (!part.is_empty() && part != b"." && part != b"..")),
            "{stage} program must be an absolute or normalized repository-relative path"
        );
        ensure!(
            self.timeout_seconds != Some(0),
            "{stage} timeout_seconds must be positive"
        );
        ensure!(
            !self.program.as_os_str().as_encoded_bytes().contains(&0)
                && self.args.iter().all(|arg| !arg.contains('\0')),
            "{stage} program and arguments must not contain NUL"
        );
        for (name, value) in &self.env {
            let mut bytes = name.bytes();
            ensure!(
                bytes
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
                    && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
                "{stage} has an invalid environment name: {name}"
            );
            ensure!(
                !name.starts_with("TELETE_"),
                "{stage} cannot override reserved environment: {name}"
            );
            ensure!(
                !value.contains('\0'),
                "{stage} environment value contains NUL: {name}"
            );
        }
        Ok(())
    }

    pub(crate) fn program(&self, checkout: &Path) -> PathBuf {
        if self.program.is_absolute() {
            self.program.clone()
        } else {
            checkout.join(&self.program)
        }
    }
}

fn empty_parameters() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchConfig {
    pub target: String,
    pub base: String,
    pub compose: WorkflowCommand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_repair: Option<WorkflowCommand>,
}

impl BatchConfig {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.target.is_empty()
                && self.target.len() <= 128
                && self.target.as_bytes()[0].is_ascii_alphanumeric()
                && self
                    .target
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "batch target must be a slug of 1 to 128 ASCII letters, digits, hyphens, or underscores, starting with a letter or digit"
        );
        ensure!(
            !self.base.is_empty() && !self.base.contains('\0'),
            "batch base must be a nonempty Git revision without NUL"
        );
        self.compose.validate("compose")?;
        if let Some(command) = &self.merge_repair {
            command.validate("merge_repair")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IntegrationMode {
    #[default]
    Automatic,
    Manual,
    Off,
}

fn default_integration_branch() -> String {
    "main".into()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IntegrationConfig {
    #[serde(default = "default_integration_branch")]
    pub branch: String,
    #[serde(default)]
    pub mode: IntegrationMode,
}

impl Default for IntegrationConfig {
    fn default() -> Self {
        Self {
            branch: default_integration_branch(),
            mode: IntegrationMode::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Workflow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<WorkflowCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyze: Option<WorkflowCommand>,
    pub check: WorkflowCommand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<WorkflowCommand>,
    #[serde(default)]
    pub prepare: Option<WorkflowCommand>,
    #[serde(default)]
    pub deploy: Option<WorkflowCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<BatchConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<IntegrationConfig>,
    #[serde(default = "empty_parameters")]
    pub parameters: serde_json::Value,
}

impl Workflow {
    pub(crate) fn integration(&self) -> Option<&IntegrationConfig> {
        self.integration
            .as_ref()
            .filter(|config| config.mode != IntegrationMode::Off)
    }

    pub(crate) fn command(&self, stage: &str) -> Result<&WorkflowCommand> {
        match stage {
            "fix" => self.fix.as_ref().context("automatic fixes not selected"),
            "analyze" => self.analyze.as_ref().context("analysis not selected"),
            "check" => Ok(&self.check),
            "repair" => self
                .repair
                .as_ref()
                .context("automatic repair not selected"),
            "compose" => self
                .batch
                .as_ref()
                .map(|batch| &batch.compose)
                .context("release batching not selected"),
            "merge_repair" => self
                .batch
                .as_ref()
                .and_then(|batch| batch.merge_repair.as_ref())
                .context("merge repair not selected"),
            "prepare" => self.prepare.as_ref().context("preparation not selected"),
            "deploy" => self.deploy.as_ref().context("deployment not selected"),
            _ => bail!("unknown workflow stage"),
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.parameters.is_object(),
            "workflow parameters must be an object"
        );
        if let Some(command) = &self.fix {
            command.validate("fix")?;
        }
        if let Some(command) = &self.analyze {
            command.validate("analyze")?;
        }
        self.check.validate("check")?;
        if let Some(command) = &self.repair {
            command.validate("repair")?;
        }
        if let Some(command) = &self.prepare {
            command.validate("prepare")?;
        }
        if let Some(command) = &self.deploy {
            command.validate("deploy")?;
        }
        if let Some(batch) = &self.batch {
            ensure!(self.deploy.is_some(), "batch requires deployment");
            batch.validate()?;
        }
        if let Some(integration) = self.integration() {
            ensure!(self.deploy.is_some(), "integration requires deployment");
            ensure!(
                !integration.branch.is_empty() && !integration.branch.contains('\0'),
                "integration branch must be a literal local Git branch name"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueueMode {
    #[default]
    Global,
    Repository,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectConfig {
    pub repo: PathBuf,
    pub state: PathBuf,
    pub queue: QueueMode,
    pub workflow: Workflow,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    repo: PathBuf,
    state: PathBuf,
    #[serde(default)]
    queue: QueueMode,
    #[serde(default)]
    fix: Option<WorkflowCommand>,
    #[serde(default)]
    analyze: Option<WorkflowCommand>,
    check: WorkflowCommand,
    #[serde(default)]
    repair: Option<WorkflowCommand>,
    #[serde(default)]
    prepare: Option<WorkflowCommand>,
    #[serde(default)]
    deploy: Option<WorkflowCommand>,
    #[serde(default)]
    batch: Option<BatchConfig>,
    #[serde(default)]
    integration: Option<IntegrationConfig>,
    #[serde(default = "empty_parameters")]
    parameters: serde_json::Value,
}

pub(crate) fn normalized_absolute(path: &Path, directory: &Path) -> PathBuf {
    let joined = directory.join(path);
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

pub(crate) fn read(path: &Path) -> Result<ProjectConfig> {
    ensure!(!path.as_os_str().is_empty(), "config path is empty");
    let path = normalized_absolute(path, &std::env::current_dir()?);
    ensure!(
        std::fs::metadata(&path)?.is_file(),
        "config must be a regular JSON file"
    );
    let file: ProjectFile = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("read config {}", path.display()))?,
    )
    .with_context(|| format!("decode config {}", path.display()))?;
    ensure!(
        !file.repo.as_os_str().is_empty(),
        "config repo path is empty"
    );
    ensure!(
        !file.state.as_os_str().is_empty(),
        "config state path is empty"
    );
    // Only a newly selected project gets the default. A retained workflow with
    // no integration field predates branch publication and keeps its semantics.
    let integration = file
        .integration
        .or_else(|| file.deploy.as_ref().map(|_| IntegrationConfig::default()))
        .filter(|config| config.mode != IntegrationMode::Off);
    let workflow = Workflow {
        fix: file.fix,
        analyze: file.analyze,
        check: file.check,
        repair: file.repair,
        prepare: file.prepare,
        deploy: file.deploy,
        batch: file.batch,
        integration,
        parameters: file.parameters,
    };
    workflow.validate()?;
    let directory = path.parent().context("config has no parent directory")?;
    Ok(ProjectConfig {
        repo: normalized_absolute(&file.repo, directory),
        state: normalized_absolute(&file.state, directory),
        queue: file.queue,
        workflow,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StageRun {
    pub stage: String,
    pub commit: CommitId,
    pub operation: String,
    pub command: CommandSpec,
    pub result: ProcessResult,
    pub stamp: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workflow(value: serde_json::Value) -> Workflow {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn commands_keep_literal_arguments_and_allow_project_environment() -> Result<()> {
        let workflow = workflow(json!({
            "check":{"program":"scripts/check","args":["one two", "$(touch /tmp/no)", "", "--x=y"],
                "env":{"HOME":"/project/home","CARGO_TARGET_DIR":"/project/target","PATH":"relative:/bin"}},
            "prepare":{"program":"scripts/prepare"},
            "deploy":{"program":"scripts/deploy"}
        }));
        workflow.validate()?;
        assert_eq!(workflow.parameters, json!({}));
        assert_eq!(
            workflow.check.args,
            ["one two", "$(touch /tmp/no)", "", "--x=y"]
        );
        assert_eq!(
            workflow.check.program(Path::new("/checkout")),
            Path::new("/checkout/scripts/check")
        );
        assert!(workflow.deploy.is_some());
        Ok(())
    }

    #[test]
    fn deployment_does_not_require_preparation() -> Result<()> {
        workflow(json!({"check":{"program":"check"},"deploy":{"program":"deploy"}})).validate()
    }

    #[test]
    fn project_defaults_integration_for_new_deployments_only() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config = directory.path().join("telete.json");
        let mut value = json!({
            "repo": ".", "state": ".telete",
            "check": {"program": "check"},
            "deploy": {"program": "deploy"},
        });
        std::fs::write(&config, serde_json::to_vec(&value)?)?;
        assert_eq!(
            read(&config)?.workflow.integration(),
            Some(&IntegrationConfig::default())
        );
        assert!(
            workflow(json!({
                "check": {"program": "check"},
                "deploy": {"program": "deploy"},
            }))
            .integration()
            .is_none()
        );

        value["integration"] = json!({"mode": "manual", "branch": "release/main"});
        std::fs::write(&config, serde_json::to_vec(&value)?)?;
        assert_eq!(
            read(&config)?.workflow.integration(),
            Some(&IntegrationConfig {
                branch: "release/main".into(),
                mode: IntegrationMode::Manual,
            })
        );

        value["integration"] = json!({"mode": "off"});
        std::fs::write(&config, serde_json::to_vec(&value)?)?;
        assert!(read(&config)?.workflow.integration().is_none());

        value.as_object_mut().unwrap().remove("integration");
        value.as_object_mut().unwrap().remove("deploy");
        std::fs::write(&config, serde_json::to_vec(&value)?)?;
        assert!(read(&config)?.workflow.integration().is_none());
        Ok(())
    }

    #[test]
    fn batching_requires_deployment_and_validates_its_command_and_identity() -> Result<()> {
        let value = json!({
            "check": {"program": "check"},
            "deploy": {"program": "deploy"},
            "batch": {
                "target": "production-mac_1",
                "base": "refs/heads/main",
                "compose": {"program": "ci/compose"},
            },
        });
        let selected = workflow(value.clone());
        selected.validate()?;
        assert_eq!(
            selected.command("compose")?.program,
            Path::new("ci/compose")
        );
        for target in ["", "-release", "prod/release", "prod release", ".", "é"] {
            let mut invalid = value.clone();
            invalid["batch"]["target"] = json!(target);
            assert!(workflow(invalid).validate().is_err(), "{target}");
        }
        let mut invalid = value.clone();
        invalid["batch"]["target"] = json!("a".repeat(129));
        assert!(workflow(invalid).validate().is_err());
        for base in ["", "\0"] {
            let mut invalid = value.clone();
            invalid["batch"]["base"] = json!(base);
            assert!(workflow(invalid).validate().is_err());
        }
        let mut invalid = value.clone();
        invalid["deploy"] = serde_json::Value::Null;
        assert!(workflow(invalid).validate().is_err());
        for command in [
            json!({"program": "../compose"}),
            json!({"program": "compose", "env": {"TELETE_CONTEXT": "elsewhere"}}),
            json!({"program": "compose", "timeout_seconds": 0}),
        ] {
            let mut invalid = value.clone();
            invalid["batch"]["compose"] = command;
            assert!(workflow(invalid).validate().is_err());
        }
        let mut invalid = value.clone();
        invalid["batch"]["unknown"] = json!(true);
        assert!(serde_json::from_value::<Workflow>(invalid).is_err());
        Ok(())
    }

    #[test]
    fn project_config_reads_repair_and_batch_selection() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("telete.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "repo": ".", "state": ".telete",
                "check": {"program": "check"},
                "repair": {"program": "repair"},
                "deploy": {"program": "deploy"},
                "batch": {
                    "target": "production", "base": "main",
                    "compose": {"program": "compose"},
                },
            }))?,
        )?;
        let project = read(&path)?;
        assert_eq!(
            project.workflow.command("repair")?.program,
            Path::new("repair")
        );
        let batch = project.workflow.batch.context("selected batch")?;
        assert_eq!(batch.target, "production");
        assert_eq!(batch.base, "main");
        assert_eq!(batch.compose.program, Path::new("compose"));
        Ok(())
    }

    #[test]
    fn ambiguous_paths_invalid_environment_and_zero_timeout_are_rejected() {
        for program in [
            "",
            ".",
            "./check",
            "../check",
            "scripts/../check",
            "scripts//check",
            "scripts/./check",
            "check/",
        ] {
            assert!(
                workflow(json!({"check":{"program":program}}))
                    .validate()
                    .is_err(),
                "{program}"
            );
        }
        for command in [
            json!({"program":"check","timeout_seconds":0}),
            json!({"program":"check","args":["\u{0}"]}),
            json!({"program":"check","env":{"TELETE_CONTEXT":"elsewhere"}}),
            json!({"program":"check","env":{"BAD=NAME":"value"}}),
            json!({"program":"check","env":{"1BAD":"value"}}),
            json!({"program":"check","env":{"VALID":"\u{0}"}}),
        ] {
            assert!(workflow(json!({"check":command})).validate().is_err());
        }
    }

    #[test]
    fn config_paths_are_relative_to_config_and_old_fields_are_rejected() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config_dir = directory.path().join("config");
        std::fs::create_dir(&config_dir)?;
        let path = config_dir.join("telete.json");
        std::fs::write(
            &path,
            r#"{"repo":"..","state":"../.telete","check":{"program":"check"}}"#,
        )?;
        let project = read(&path)?;
        assert_eq!(project.repo, directory.path());
        assert_eq!(project.state, directory.path().join(".telete"));
        for value in [
            json!({"check":{"program":"check"}}),
            json!({"repo":"..","state":"../.telete","check":{"program":"check"},"integrate":true}),
            json!({"repo":"..","state":"../.telete","check":{"program":"check"},"accept":true}),
            json!({"repo":"..","state":"../.telete","check":{"program":"check"},"unknown":true}),
        ] {
            std::fs::write(&path, serde_json::to_vec(&value)?)?;
            assert!(read(&path).is_err());
        }
        Ok(())
    }
}
