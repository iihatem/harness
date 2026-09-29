//! The signed-in ChatGPT account a `chatgpt/*` model uses: its tokens, refreshed when the access
//! token expires within five minutes and after a 401, and stored again whenever they change.
//! Refresh tokens are single-use, so before asking for new tokens the stored ones are read
//! again: another harness process may already have refreshed them.

use std::{sync::Arc, time::Duration};

use harness_core::{provider::ProviderError, redact::Redactor, time::now_unix};
use tokio::sync::Mutex;

use super::oauth::{OAuth, OAuthError, Tokens};
use crate::credentials::{CredentialError, Credentials};

/// The provider name of the ChatGPT account.
pub const PROVIDER: &str = "chatgpt";
/// Where `chatgpt/*` requests go.
pub const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Refresh the access token when it expires within this long.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

pub struct ChatGptAuth {
    oauth: OAuth,
    credentials: Arc<Credentials>,
    profile: String,
    tokens: Mutex<Tokens>,
    /// Where the tokens, and those that replace them, are registered as secrets.
    redactor: Option<Arc<Redactor>>,
}

impl ChatGptAuth {
    /// The account signed in under `profile`, or `None` when there is none.
    pub fn load(
        credentials: Arc<Credentials>,
        profile: &str,
        oauth: OAuth,
    ) -> Result<Option<ChatGptAuth>, CredentialError> {
        let Some(tokens) = stored(&credentials, profile)? else {
            return Ok(None);
        };
        Ok(Some(ChatGptAuth {
            oauth,
            credentials,
            profile: profile.to_string(),
            tokens: Mutex::new(tokens),
            redactor: None,
        }))
    }

    /// Registers the tokens, now and after each refresh, as secrets with `redactor`.
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> ChatGptAuth {
        let tokens = self.tokens.get_mut();
        redactor.add(&tokens.access_token);
        redactor.add(&tokens.refresh_token);
        self.redactor = Some(redactor);
        self
    }

    /// The tokens for the next request, refreshed first when the access token is about to
    /// expire.
    pub async fn current(&self) -> Result<Tokens, ProviderError> {
        let mut tokens = self.tokens.lock().await;
        if expiring(&tokens) {
            let used = tokens.access_token.clone();
            self.renew(&mut tokens, &used).await?;
        }
        Ok(tokens.clone())
    }

    /// The tokens to retry with after the server refused `used` with a 401.
    pub async fn after_unauthorized(&self, used: &str) -> Result<Tokens, ProviderError> {
        let mut tokens = self.tokens.lock().await;
        // Another request of this process already renewed them.
        if tokens.access_token != used {
            return Ok(tokens.clone());
        }
        self.renew(&mut tokens, used).await?;
        Ok(tokens.clone())
    }

    /// Registers `tokens` as secrets.
    fn register(&self, tokens: &Tokens) {
        if let Some(redactor) = &self.redactor {
            redactor.add(&tokens.access_token);
            redactor.add(&tokens.refresh_token);
        }
    }

    /// Replaces `tokens`, whose access token `used` is no good: with the stored tokens when
    /// another process has renewed them, else with refreshed ones, which are then stored.
    async fn renew(&self, tokens: &mut Tokens, used: &str) -> Result<(), ProviderError> {
        if let Ok(Some(theirs)) = stored(&self.credentials, &self.profile)
            && theirs.access_token != used
        {
            self.register(&theirs);
            *tokens = theirs;
            if !expiring(tokens) {
                return Ok(());
            }
        }
        let fresh = self.oauth.refresh(tokens).await.map_err(refresh_error)?;
        self.register(&fresh);
        self.credentials
            .set(PROVIDER, &self.profile, &fresh.to_json())
            .map_err(|e| {
                ProviderError::Protocol(format!("cannot store the refreshed tokens: {e}"))
            })?;
        *tokens = fresh;
        Ok(())
    }
}

/// The tokens stored under `profile`, if they are there and readable.
fn stored(credentials: &Credentials, profile: &str) -> Result<Option<Tokens>, CredentialError> {
    Ok(credentials
        .get(PROVIDER, profile)?
        .as_deref()
        .and_then(Tokens::from_json))
}

/// Whether the access token expires within [`REFRESH_MARGIN`]. A token without an expiry is used
/// until the server refuses it.
fn expiring(tokens: &Tokens) -> bool {
    tokens
        .expires_at()
        .is_some_and(|at| at <= now_unix() + REFRESH_MARGIN.as_secs())
}

/// A failed refresh as the provider's error: an unreachable server can be retried; a refusal
/// means signing in again, and reads as the 401 it stands for.
fn refresh_error(error: OAuthError) -> ProviderError {
    match error {
        OAuthError::Network(message) => ProviderError::Network(message),
        other => ProviderError::Http {
            status: 401,
            body: other.to_string(),
            retry_after: None,
        },
    }
}
