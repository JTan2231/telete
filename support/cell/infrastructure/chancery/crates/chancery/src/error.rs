use std::fmt;

#[derive(Debug)]
pub(crate) struct AppError {
    code: &'static str,
    message: String,
    exit_code: i32,
}

impl AppError {
    pub(crate) fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            exit_code: 1,
        }
    }

    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_command",
            message: message.into(),
            exit_code: 2,
        }
    }

    #[must_use]
    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub(crate) fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}
