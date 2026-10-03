//! Asking ChatGPT where the account's usage windows stand: `GET /wham/usage`.

use std::time::Duration;

use harness_core::{meter::WindowSnapshot, time::now_unix};
use serde_json::Value;

use super::{auth::ChatGptAuth, oauth::Tokens};
use crate::codex_windows::from_usage_body;

/// How long the poll may take.
const TIMEOUT: Duration = Duration::from_secs(20);

/// The usage endpoint for a backend at `base_url` (`…/backend-api/codex`): `…/backend-api/wham/usage`.
pub fn usage_url(base_url: &str) -> String {
    let root = base_url.trim_end_matches('/');
    let root = root.strip_suffix("/codex").unwrap_or(root);
    format!("{root}/wham/usage")
}

fn request(client: &reqwest::Client, url: &str, tokens: &Tokens) -> reqwest::RequestBuilder {
    let mut request = client
        .get(url)
        .timeout(TIMEOUT)
        .bearer_auth(&tokens.access_token)
        .header("originator", super::oauth::ORIGINATOR);
    if let Some(account) = &tokens.account_id {
        request = request.header("ChatGPT-Account-ID", account);
    }
    request
}

/// Asks for the windows as the signed-in account. A 401 renews the tokens once. Errors say what
/// happened, never what the server sent.
pub async fn poll(
    client: &reqwest::Client,
    base_url: &str,
    auth: &ChatGptAuth,
) -> Result<WindowSnapshot, String> {
    let url = usage_url(base_url);
    let tokens = auth.current().await.map_err(|e| e.to_string())?;
    let mut response = request(client, &url, &tokens).send().await.map_err(|e| {
        format!(
            "cannot reach the usage endpoint: {}",
            crate::http::describe(&e.without_url())
        )
    })?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        let tokens = auth
            .after_unauthorized(&tokens.access_token)
            .await
            .map_err(|e| e.to_string())?;
        response = request(client, &url, &tokens).send().await.map_err(|e| {
            format!(
                "cannot reach the usage endpoint: {}",
                crate::http::describe(&e.without_url())
            )
        })?;
    }
    if !response.status().is_success() {
        return Err(format!(
            "the usage endpoint answered HTTP {}",
            response.status().as_u16()
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| "the usage endpoint's answer could not be read".to_string())?;
    Ok(from_usage_body(&body, now_unix()))
}
