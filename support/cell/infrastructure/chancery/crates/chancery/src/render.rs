use std::fmt::Write as _;

use serde_json::Value;

use crate::api::{ErrorBody, ErrorOutput, Output};
use crate::error::AppError;
use crate::model::OUTPUT_SCHEMA_VERSION;

#[derive(Debug)]
pub(crate) struct CommandOutput {
    pub(crate) ok: bool,
    pub(crate) data: Value,
    pub(crate) human: String,
    pub(crate) exit_code: i32,
}

impl CommandOutput {
    pub(crate) fn success(data: Value, human: impl Into<String>) -> Self {
        Self {
            ok: true,
            data,
            human: human.into(),
            exit_code: 0,
        }
    }

    pub(crate) fn report(ok: bool, data: Value, human: impl Into<String>, exit_code: i32) -> Self {
        Self {
            ok,
            data,
            human: human.into(),
            exit_code,
        }
    }
}

pub(crate) fn output_json(output: &CommandOutput) -> Result<String, AppError> {
    serde_json::to_string(&Output {
        schema_version: OUTPUT_SCHEMA_VERSION,
        ok: output.ok,
        data: &output.data,
    })
    .map_err(|error| {
        AppError::invalid(
            "json_serialization_failed",
            format!("unable to serialize output: {error}"),
        )
    })
}

#[must_use]
pub(crate) fn error_json(error: &AppError) -> String {
    serde_json::to_string(&ErrorOutput {
        schema_version: OUTPUT_SCHEMA_VERSION,
        ok: false,
        error: ErrorBody {
            code: error.code().to_owned(),
            message: error.to_string(),
        },
    })
    .unwrap_or_else(|_| {
        format!(
            "{{\"schema_version\":{OUTPUT_SCHEMA_VERSION},\"ok\":false,\"error\":{{\"code\":\"json_serialization_failed\",\"message\":\"unable to serialize error\"}}}}"
        )
    })
}

/// Render provider-controlled text without allowing terminal control sequences.
#[must_use]
pub(crate) fn terminal_text(value: &str, allow_newlines: bool) -> String {
    let mut rendered = String::with_capacity(value.len());
    for character in value.chars() {
        if character == '\n' && allow_newlines {
            rendered.push(character);
        } else if character.is_control() {
            let _ = write!(rendered, "\\u{{{:x}}}", u32::from(character));
        } else {
            rendered.push(character);
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::terminal_text;

    #[test]
    fn terminal_controls_are_escaped() {
        assert_eq!(
            terminal_text("first\n\u{1b}[31m", true),
            "first\n\\u{1b}[31m"
        );
    }
}
