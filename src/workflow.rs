//! Caller-selected project commands, frozen with each submission.

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
    fn validate(&self, stage: &str, host: bool) -> Result<()> {
        ensure!(
            !self.program.as_os_str().is_empty(),
            "{stage} program is empty"
        );
        ensure!(
            self.program
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
            "{stage} program must be an absolute or normalized repository-relative path"
        );
        // Path components omit interior `.` and repeated separators. Check the
        // source spelling too, so the retained selection has one clear path.
        ensure!(
            self.program
                .as_os_str()
                .as_encoded_bytes()
                .split(|byte| *byte == b'/')
                .enumerate()
                .all(|(index, part)| (index == 0 && self.program.is_absolute())
                    || (!part.is_empty() && part != b"." && part != b"..")),
            "{stage} program must be an absolute or normalized repository-relative path"
        );
        ensure!(
            !host || self.program.is_absolute(),
            "{stage} program must be an absolute trusted host path"
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
                !name.starts_with("TELETE_")
                    && !matches!(
                        name.as_str(),
                        "HOME"
                            | "TMPDIR"
                            | "TMP"
                            | "TEMP"
                            | "CARGO_HOME"
                            | "CARGO_TARGET_DIR"
                            | "RUSTUP_HOME"
                            | "XDG_CACHE_HOME"
                            | "CLANG_MODULE_CACHE_PATH"
                            | "SWIFT_MODULECACHE_PATH"
                            | "CHANCERY_USAGE_INTERNAL"
                    ),
                "{stage} cannot override reserved environment: {name}"
            );
            ensure!(
                !value.contains('\0'),
                "{stage} environment value contains NUL: {name}"
            );
            if name == "PATH" {
                ensure!(
                    std::env::split_paths(value).all(|directory| directory.is_absolute()),
                    "{stage} PATH must contain only absolute nonempty directories"
                );
            }
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
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Workflow {
    #[serde(default)]
    pub integrate: bool,
    #[serde(default)]
    pub accept: bool,
    pub check: WorkflowCommand,
    #[serde(default)]
    pub prepare: Option<WorkflowCommand>,
    #[serde(default)]
    pub deploy: Option<WorkflowCommand>,
    #[serde(default = "empty_parameters")]
    pub parameters: serde_json::Value,
    #[serde(default)]
    pub repair: bool,
    #[serde(default)]
    pub notify: bool,
}

impl Workflow {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.parameters.is_object(),
            "workflow parameters must be an object"
        );
        ensure!(
            !self.accept || self.integrate,
            "workflow acceptance requires integration"
        );
        ensure!(
            self.deploy.is_none() || self.prepare.is_some(),
            "workflow deployment requires preparation"
        );
        self.check.validate("check", false)?;
        if let Some(command) = &self.prepare {
            command.validate("prepare", true)?;
        }
        if let Some(command) = &self.deploy {
            command.validate("deploy", true)?;
        }
        Ok(())
    }
}

pub(crate) fn read(path: Option<&Path>) -> Result<Option<Workflow>> {
    let Some(path) = path else { return Ok(None) };
    ensure!(path.is_absolute(), "workflow file must be an absolute path");
    ensure!(
        std::fs::symlink_metadata(path)?.is_file(),
        "workflow must be a regular JSON file"
    );
    let workflow: Workflow = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read workflow {}", path.display()))?,
    )
    .with_context(|| format!("decode workflow {}", path.display()))?;
    workflow.validate()?;
    Ok(Some(workflow))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StageRun {
    pub stage: String,
    pub candidate: CommitId,
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
    fn check_only_defaults_and_literal_arguments_are_retained() -> Result<()> {
        let workflow = workflow(
            json!({"check":{"program":"scripts/check","args":["one two", "$(touch /tmp/no)", "", "--x=y"]}}),
        );
        workflow.validate()?;
        assert!(!workflow.integrate && !workflow.accept && !workflow.repair && !workflow.notify);
        assert_eq!(workflow.parameters, json!({}));
        assert_eq!(
            workflow.check.args,
            ["one two", "$(touch /tmp/no)", "", "--x=y"]
        );
        assert_eq!(
            workflow.check.program(Path::new("/checkout")),
            Path::new("/checkout/scripts/check")
        );
        Ok(())
    }

    #[test]
    fn invalid_stage_selections_are_rejected() {
        for value in [
            json!({"check":{"program":"check"},"accept":true}),
            json!({"check":{"program":"check"},"deploy":{"program":"/host/deploy"}}),
            json!({"check":{"program":"check"},"prepare":{"program":"prepare"}}),
            json!({"check":{"program":"check"},"prepare":{"program":"/host/prepare"},"deploy":{"program":"deploy"}}),
            json!({"check":{"program":"check"},"parameters":[]}),
        ] {
            assert!(workflow(value).validate().is_err());
        }
        assert!(workflow(json!({"integrate":true,"accept":true,"check":{"program":"check"},"prepare":{"program":"/host/prepare"},"deploy":{"program":"/host/deploy"}})).validate().is_ok());
    }

    #[test]
    fn ambiguous_paths_environment_and_zero_timeout_are_rejected() {
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
            json!({"program":"check","env":{"TELETE_STATE":"elsewhere"}}),
            json!({"program":"check","env":{"CARGO_TARGET_DIR":"/elsewhere"}}),
            json!({"program":"check","env":{"HOME":"/elsewhere"}}),
            json!({"program":"check","env":{"PATH":"/bin::/usr/bin"}}),
            json!({"program":"check","env":{"PATH":"relative:/bin"}}),
            json!({"program":"check","env":{"BAD=NAME":"value"}}),
            json!({"program":"check","env":{"1BAD":"value"}}),
            json!({"program":"check","env":{"VALID":"\u{0}"}}),
        ] {
            assert!(workflow(json!({"check":command})).validate().is_err());
        }
    }

    #[test]
    fn workflow_file_is_validated_and_unknown_selections_are_rejected() -> Result<()> {
        assert!(read(Some(Path::new("workflow.json"))).is_err());
        assert!(read(None)?.is_none());
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("workflow.json");
        std::fs::write(&path, r#"{"check":{"program":"check"},"unknown":true}"#)?;
        assert!(read(Some(&path)).is_err());
        std::fs::write(
            &path,
            r#"{"check":{"program":"check","env":{"PROJECT_MODE":"ci"}}}"#,
        )?;
        assert_eq!(read(Some(&path))?.unwrap().check.env["PROJECT_MODE"], "ci");
        Ok(())
    }
}
