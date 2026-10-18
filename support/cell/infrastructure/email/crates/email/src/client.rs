use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use serde::Serialize;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::api::{
    Attachment, Message, Receipt, ReceivedMessage, ReceivedPage, ReceivedPageRequest, ReplyOptions,
};
use crate::{AppError, AppResult};

/// Typed access through one exact installed Email executable. The child wrapper
/// loads the Email credential; this client neither reads it nor stores mail.
#[derive(Debug, Clone)]
pub struct Client {
    executable: PathBuf,
}

impl Client {
    /// Discover local receiving selection or verified provider receiving domains.
    ///
    /// # Errors
    /// Returns command failure, malformed settings or invalid domains.
    pub async fn receiving_settings(&self) -> AppResult<crate::api::ReceivingSettings> {
        let output = self
            .invoke(&["receive".into(), "settings".into()], None, 65536)
            .await?;
        let result: crate::api::ReceivingSettings = serde_json::from_slice(&output)
            .map_err(|_| AppError::new("Email returned invalid receiving settings"))?;
        for domain in &result.domains {
            crate::settings::validate_domain(domain)?;
        }
        Ok(result)
    }
    /// Select an absolute installed wrapper path. Validation occurs before use.
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    /// The selected executable; this read does not run it or check readiness.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Submit exact message and attachment bytes through the installed wrapper.
    ///
    /// # Errors
    /// Returns invalid inputs, process failure, timeout or an invalid receipt.
    pub async fn send_with_options(
        &self,
        message: &Message,
        attachments: &[Attachment],
        options: &ReplyOptions,
    ) -> AppResult<Receipt> {
        crate::validate_reply_options(options)?;
        let mut arguments = vec!["--payload-stdin".to_owned()];
        if let Some(key) = &message.idempotency_key {
            crate::parse_idempotency_key(key).map_err(AppError::new)?;
            arguments.extend(["--idempotency-key".to_owned(), key.clone()]);
        }
        if let Some(address) = &options.reply_to {
            arguments.extend(["--reply-to".to_owned(), address.clone()]);
        }
        if let Some(id) = &options.in_reply_to {
            arguments.extend(["--in-reply-to".to_owned(), id.clone()]);
        }
        for id in &options.references {
            arguments.extend(["--reference".to_owned(), id.clone()]);
        }
        arguments.extend(["--".to_owned(), message.subject.clone(), "-".to_owned()]);
        let mut encoded = Vec::new();
        for attachment in attachments {
            crate::validate_attachment_filename(&attachment.filename)?;
            encoded.push(PayloadAttachment {
                filename: &attachment.filename,
                content: base64::engine::general_purpose::STANDARD.encode(&attachment.content),
            });
        }
        let payload = serde_json::to_vec(&Payload {
            body: &message.body,
            attachments: encoded,
        })
        .map_err(|_| AppError::new("unable to encode Email payload"))?;
        let output = self.invoke(&arguments, Some(payload), 4096).await?;
        let output = std::str::from_utf8(&output)
            .map_err(|_| AppError::new("Email returned an invalid acceptance receipt"))?;
        let id = output
            .trim_end_matches(['\r', '\n'])
            .strip_prefix("Accepted ")
            .ok_or_else(|| AppError::new("Email returned an invalid acceptance receipt"))?;
        if id.is_empty() || id.len() > 256 || !id.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(AppError::new(
                "Email returned an invalid acceptance receipt",
            ));
        }
        Ok(Receipt { id: id.to_owned() })
    }

    /// Read one account metadata page without retaining or acknowledging it.
    ///
    /// # Errors
    /// Returns invalid page input, command failure or invalid JSON output.
    pub async fn list_received(&self, page: &ReceivedPageRequest) -> AppResult<ReceivedPage> {
        crate::receiving::validate_page(page)?;
        let mut arguments = vec![
            "receive".to_owned(),
            "list".to_owned(),
            "--limit".to_owned(),
            page.limit.to_string(),
        ];
        if let Some(id) = &page.after {
            arguments.extend(["--after".to_owned(), id.clone()]);
        }
        let output = self
            .invoke(&arguments, None, MAX_COMMAND_OUTPUT_BYTES)
            .await?;
        let result: ReceivedPage = serde_json::from_slice(&output)
            .map_err(|_| AppError::new("Email returned an invalid received email page"))?;
        if result.data.len() > usize::from(page.limit)
            || (result.has_more && result.data.is_empty())
        {
            return Err(AppError::new(
                "Email returned an invalid received email page",
            ));
        }
        Ok(result)
    }

    /// Retrieve one account email without downloading attachments or remote content.
    ///
    /// # Errors
    /// Returns invalid ID, command failure or invalid JSON output.
    pub async fn get_received(&self, id: &str) -> AppResult<ReceivedMessage> {
        crate::receiving::validate_id(id)?;
        let arguments = vec!["receive".to_owned(), "get".to_owned(), id.to_owned()];
        let output = self
            .invoke(&arguments, None, MAX_COMMAND_OUTPUT_BYTES)
            .await?;
        let message: ReceivedMessage = serde_json::from_slice(&output)
            .map_err(|_| AppError::new("Email returned invalid received email data"))?;
        if message.id != id {
            return Err(AppError::new(
                "Email returned a different received email ID",
            ));
        }
        Ok(message)
    }

    async fn invoke(
        &self,
        arguments: &[String],
        input: Option<Vec<u8>>,
        limit: usize,
    ) -> AppResult<Vec<u8>> {
        if !self.executable.is_absolute() {
            return Err(AppError::new(
                "Email executable must be an absolute installed wrapper path",
            ));
        }
        let home = std::env::var_os("HOME")
            .ok_or_else(|| AppError::new("HOME must be set for the installed Email wrapper"))?;
        let mut child = tokio::process::Command::new(&self.executable)
            .args(arguments)
            .env_clear()
            .env("CHANCERY_USAGE_INTERNAL", "1")
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| AppError::new("unable to start installed Email command"))?;
        let operation = async {
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| AppError::new("unable to read Email output"))?;
            let write = async {
                if let Some(input) = input {
                    let mut stdin = child
                        .stdin
                        .take()
                        .ok_or_else(|| AppError::new("unable to open Email input"))?;
                    stdin
                        .write_all(&input)
                        .await
                        .map_err(|_| AppError::new("unable to supply Email input"))?;
                    stdin
                        .shutdown()
                        .await
                        .map_err(|_| AppError::new("unable to finish Email input"))?;
                }
                Ok::<_, AppError>(())
            };
            let read = async {
                let mut output = Vec::new();
                let mut buffer = [0_u8; 8192];
                loop {
                    let count = stdout
                        .read(&mut buffer)
                        .await
                        .map_err(|_| AppError::new("unable to read Email output"))?;
                    if count == 0 {
                        break;
                    }
                    if count > limit.saturating_sub(output.len()) {
                        return Err(AppError::new("Email command output exceeds its byte limit"));
                    }
                    output.extend_from_slice(&buffer[..count]);
                }
                Ok::<_, AppError>(output)
            };
            let ((), output) = tokio::try_join!(write, read)?;
            let status = child
                .wait()
                .await
                .map_err(|_| AppError::new("unable to observe Email command completion"))?;
            if !status.success() {
                return Err(AppError::new(
                    "installed Email command failed; acceptance or read is not confirmed",
                ));
            }
            Ok(output)
        };
        tokio::time::timeout(Duration::from_secs(120), operation)
            .await
            .map_err(|_| {
                AppError::new("installed Email command timed out; send acceptance may be unknown")
            })?
    }
}

// Normalized JSON can grow when optional metadata fields are emitted.
const MAX_COMMAND_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize)]
struct Payload<'a> {
    body: &'a str,
    attachments: Vec<PayloadAttachment<'a>>,
}

#[derive(Serialize)]
struct PayloadAttachment<'a> {
    filename: &'a str,
    content: String,
}
