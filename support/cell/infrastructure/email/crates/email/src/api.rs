//! Fixed-recipient submission and stateless reads from the Resend receiving account.

pub use crate::AppError as Error;
pub use crate::client::Client;
pub use crate::settings::ReceivingSettings;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Email's fixed personal recipient. A caller can compare an incoming sender
/// with this address; the comparison alone does not authenticate that sender.
#[must_use]
pub fn recipient() -> &'static str {
    crate::TO
}

/// Email's fixed outbound sender, including its display name.
#[must_use]
pub fn sender() -> &'static str {
    crate::FROM
}

/// The exact message and optional caller-owned occurrence key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub subject: String,
    pub body: String,
    pub idempotency_key: Option<String>,
}

/// One caller-owned file attachment, already captured as exact bytes.
/// Only the filename and content are disclosed; local paths are not transmitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub filename: String,
    pub content: Vec<u8>,
}

/// Optional reply routing and RFC message identifiers for one exact send.
/// These fields do not change Email's fixed sender or recipient.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyOptions {
    pub reply_to: Option<String>,
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
}

/// One bounded provider page, ordered from newer to older records.
/// `after` is the last provider ID from the preceding page, not an acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivedPageRequest {
    pub limit: u16,
    pub after: Option<String>,
}

impl Default for ReceivedPageRequest {
    fn default() -> Self {
        Self {
            limit: 100,
            after: None,
        }
    }
}

/// One provider page. A list contains metadata, not message bodies or headers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivedPage {
    pub data: Vec<ReceivedMessage>,
    pub has_more: bool,
}

/// Provider email data. `id` selects a Resend record; `message_id` identifies
/// the RFC email for threading. All addresses, headers and content are untrusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivedMessage {
    pub id: String,
    pub message_id: String,
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    pub created_at: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub attachments: Vec<ReceivedAttachment>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub reply_to: Vec<String>,
    #[serde(default)]
    pub received_for: Vec<String>,
}

/// Attachment metadata only. Email does not download receiving attachments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivedAttachment {
    pub id: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub content_id: Option<String>,
    pub content_disposition: Option<String>,
    pub size: Option<u64>,
}

/// Resend accepted the submission; this is not a final-delivery receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub id: String,
}

impl std::fmt::Display for Receipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Accepted {}", self.id)
    }
}

/// Submit a message using Email's fixed addresses and existing environment credential.
/// The caller owns authorization and the exact payload associated with a supplied key.
///
/// # Errors
/// Returns the existing input, credential or bounded transport failure.
pub async fn send(message: &Message) -> Result<Receipt, Error> {
    send_with_attachments(message, &[]).await
}

/// Submit a fixed-recipient message with captured file attachments.
/// Attachment order, names, and bytes form part of the exact idempotent payload.
/// The caller owns authorization for disclosing each attachment as well as the body.
///
/// # Errors
/// Returns an invalid attachment filename, input, credential, or transport failure.
pub async fn send_with_attachments(
    message: &Message,
    attachments: &[Attachment],
) -> Result<Receipt, Error> {
    send_with_options(message, attachments, &ReplyOptions::default()).await
}

/// Submit one exact fixed-recipient message with optional reply headers.
/// The caller owns reply-address authorization and the frozen full payload.
///
/// # Errors
/// Returns an invalid attachment, reply header, key, credential, or transport error.
pub async fn send_with_options(
    message: &Message,
    attachments: &[Attachment],
    options: &ReplyOptions,
) -> Result<Receipt, Error> {
    crate::validate_reply_options(options)?;
    for attachment in attachments {
        crate::validate_attachment_filename(&attachment.filename)?;
    }
    let key = match &message.idempotency_key {
        Some(key) => crate::parse_idempotency_key(key).map_err(Error::new)?,
        None => crate::new_idempotency_key(),
    };
    let api_key = crate::resend_api_key()?;
    let id = crate::send_to_with_options(
        crate::RESEND_ENDPOINT,
        &api_key,
        &key,
        &message.subject,
        &message.body,
        attachments,
        options,
    )
    .await?;
    Ok(Receipt { id })
}

/// Read one metadata page from the receiving account using the environment credential.
/// The caller owns authorization, pagination, routing, deduplication and retention.
///
/// # Errors
/// Returns invalid page input, credential, response-limit, provider or transport errors.
pub async fn list_received(page: &ReceivedPageRequest) -> Result<ReceivedPage, Error> {
    crate::receiving::list_received(page).await
}

/// Read one received email without acknowledging, deleting or retaining it locally.
/// HTML keeps CID references; no remote content or attachments are fetched.
///
/// # Errors
/// Returns invalid ID, credential, response-limit, provider or transport errors.
pub async fn get_received(id: &str) -> Result<ReceivedMessage, Error> {
    crate::receiving::get_received(id).await
}

/// Read configured receiving domains without reading or retaining any email.
///
/// # Errors
/// Rejects malformed local settings, incomplete provider pages or unavailable access.
pub async fn receiving_settings() -> Result<ReceivingSettings, Error> {
    crate::settings::receiving_settings().await
}
