//! Configured project commands, frozen with each submission.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
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
pub(crate) struct Workflow {
    pub check: WorkflowCommand,
    #[serde(default)]
    pub prepare: Option<WorkflowCommand>,
    #[serde(default)]
    pub deploy: Option<WorkflowCommand>,
    #[serde(default = "empty_parameters")]
    pub parameters: serde_json::Value,
}

impl Workflow {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.parameters.is_object(),
            "workflow parameters must be an object"
        );
        self.check.validate("check")?;
        if let Some(command) = &self.prepare {
            command.validate("prepare")?;
        }
        if let Some(command) = &self.deploy {
            command.validate("deploy")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectConfig {
    pub repo: PathBuf,
    pub state: PathBuf,
    pub workflow: Workflow,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    repo: PathBuf,
    state: PathBuf,
    check: WorkflowCommand,
    #[serde(default)]
    prepare: Option<WorkflowCommand>,
    #[serde(default)]
    deploy: Option<WorkflowCommand>,
    #[serde(default = "empty_parameters")]
    parameters: serde_json::Value,
}

fn normalized_absolute(path: &Path, directory: &Path) -> PathBuf {
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
    let workflow = Workflow {
        check: file.check,
        prepare: file.prepare,
        deploy: file.deploy,
        parameters: file.parameters,
    };
    workflow.validate()?;
    let directory = path.parent().context("config has no parent directory")?;
    Ok(ProjectConfig {
        repo: normalized_absolute(&file.repo, directory),
        state: normalized_absolute(&file.state, directory),
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
