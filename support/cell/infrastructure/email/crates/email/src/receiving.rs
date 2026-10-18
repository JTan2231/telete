use serde::de::DeserializeOwned;

use crate::api::{ReceivedMessage, ReceivedPage, ReceivedPageRequest};
use crate::{AppError, AppResult, RETRY_DELAYS, resend_api_key, resend_client};

const RECEIVING_ENDPOINT: &str = "https://api.resend.com/emails/receiving";
pub(crate) const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn validate_id(id: &str) -> AppResult<()> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(AppError::new(
            "received email ID must contain 1 to 256 ASCII letters, digits, - or _",
        ));
    }
    Ok(())
}

pub(crate) fn validate_page(page: &ReceivedPageRequest) -> AppResult<()> {
    if !(1..=100).contains(&page.limit) {
        return Err(AppError::new(
            "received email page limit must be between 1 and 100",
        ));
    }
    if let Some(id) = &page.after {
        validate_id(id)?;
    }
    Ok(())
}

pub(crate) async fn list_received(page: &ReceivedPageRequest) -> AppResult<ReceivedPage> {
    validate_page(page)?;
    let mut query = vec![("limit", page.limit.to_string())];
    if let Some(id) = &page.after {
        query.push(("after", id.clone()));
    }
    let mut result: ReceivedPage = get_json(RECEIVING_ENDPOINT, &query).await?;
    if result.data.len() > usize::from(page.limit) || (result.has_more && result.data.is_empty()) {
        return Err(AppError::new(
            "Resend returned an invalid received email page",
        ));
    }
    for message in &mut result.data {
        validate_id(&message.id)
            .map_err(|_| AppError::new("Resend returned an invalid received email ID"))?;
        message.text = None;
        message.html = None;
        message.headers.clear();
    }
    Ok(result)
}

pub(crate) async fn get_received(id: &str) -> AppResult<ReceivedMessage> {
    validate_id(id)?;
    let result: ReceivedMessage = get_json(
        &format!("{RECEIVING_ENDPOINT}/{id}"),
        &[("html_format", "cid".to_owned())],
    )
    .await?;
    if result.id != id {
        return Err(AppError::new(
            "Resend returned a different received email ID",
        ));
    }
    Ok(result)
}

pub(crate) async fn get_json<T: DeserializeOwned>(
    endpoint: &str,
    query: &[(&str, String)],
) -> AppResult<T> {
    let api_key = resend_api_key()?;
    let client = resend_client(&api_key)?;
    for attempt in 0..=RETRY_DELAYS.len() {
        let response = client.get(endpoint).query(query).send().await;
        let Ok(mut response) = response else {
            if let Some(delay) = RETRY_DELAYS.get(attempt) {
                tokio::time::sleep(*delay).await;
                continue;
            }
            return Err(AppError::new(
                "unable to read received email through Resend",
            ));
        };
        let status = response.status();
        if (status.as_u16() == 429 || status.is_server_error())
            && let Some(delay) = RETRY_DELAYS.get(attempt)
        {
            tokio::time::sleep(*delay).await;
            continue;
        }
        if !status.is_success() {
            return Err(AppError::new(format!(
                "Resend rejected receiving read with HTTP {status}"
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(AppError::new("Resend receiving response exceeds 8 MiB"));
        }
        let mut bytes = Vec::new();
        let mut interrupted = false;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                        return Err(AppError::new("Resend receiving response exceeds 8 MiB"));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => {
                    interrupted = true;
                    break;
                }
            }
        }
        if interrupted {
            if let Some(delay) = RETRY_DELAYS.get(attempt) {
                tokio::time::sleep(*delay).await;
                continue;
            }
            return Err(AppError::new(
                "unable to read complete received email response",
            ));
        }
        return serde_json::from_slice(&bytes)
            .map_err(|_| AppError::new("Resend returned invalid received email data"));
    }
    Err(AppError::new(
        "email exhausted its Resend receiving attempts",
    ))
}
