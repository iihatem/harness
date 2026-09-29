//! The signed-in ChatGPT account a `chatgpt/*` model uses: its tokens, refreshed when the access
//! token expires within five minutes and after a 401, and stored again whenever they change.
//!
//! Refresh tokens are single-use, so a renewal runs under a lock shared with other harness
//! processes (a file in the data directory, one per profile), and reads the stored tokens again
//! once it holds it: another process may have renewed them already. A renewal keeps the session's
//! account: when the profile has since been signed in to another account, the session goes on
//! with its own and leaves that sign-in alone. When the profile has been signed out since, the
//! session ends at its next renewal. Fresh tokens are used even when they cannot be stored, with a
//! warning, and storing them is tried again at the next renewal.

use std::{sync::Arc, time::Duration};

use harness_core::{provider::ProviderError, redact::Redactor, time::now_unix};
use tokio::sync::Mutex;

use super::oauth::{OAuth, OAuthError, Tokens};
use crate::{
    credentials::{CredentialError, Credentials, RenewalLock},
    registry::login_command,
};

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
    session: Mutex<Session>,
    /// Where the tokens, and those that replace them, are registered as secrets.
    redactor: Option<Arc<Redactor>>,
}

/// The session's view of the sign-in.
struct Session {
    /// The tokens requests carry.
    tokens: Tokens,
    /// What the store held when this session last read or wrote it. Stored tokens that differ
    /// were written by someone else since.
    seen: Option<Tokens>,
    /// Whether the user was told the profile is now signed in to another account.
    told_of_other_account: bool,
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
            session: Mutex::new(Session {
                seen: Some(tokens.clone()),
                tokens,
                told_of_other_account: false,
            }),
            redactor: None,
        }))
    }

    /// Registers the tokens, now and after each refresh, as secrets with `redactor`.
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> ChatGptAuth {
        let tokens = &self.session.get_mut().tokens;
        redactor.add(&tokens.access_token);
        redactor.add(&tokens.refresh_token);
        self.redactor = Some(redactor);
        self
    }

    /// The tokens for the next request, refreshed first when the access token is about to
    /// expire.
    pub async fn current(&self) -> Result<Tokens, ProviderError> {
        let mut session = self.session.lock().await;
        if expiring(&session.tokens) {
            let used = session.tokens.access_token.clone();
            self.renew(&mut session, &used).await?;
        }
        Ok(session.tokens.clone())
    }

    /// The tokens to retry with after the server refused `used` with a 401.
    pub async fn after_unauthorized(&self, used: &str) -> Result<Tokens, ProviderError> {
        let mut session = self.session.lock().await;
        // Another request of this process already renewed them.
        if session.tokens.access_token != used {
            return Ok(session.tokens.clone());
        }
        self.renew(&mut session, used).await?;
        Ok(session.tokens.clone())
    }

    /// `error`, the server's answer to renewed tokens, with what to do: sign in again.
    pub fn still_refused(&self, error: ProviderError) -> ProviderError {
        match error {
            ProviderError::Http {
                status: 401,
                body,
                retry_after,
            } => ProviderError::Http {
                status: 401,
                body: format!(
                    "{body} (the ChatGPT sign-in was renewed and is still refused: run `{}` to sign in again)",
                    login_command(PROVIDER, &self.profile)
                ),
                retry_after,
            },
            other => other,
        }
    }

    /// Registers `tokens` as secrets.
    fn register(&self, tokens: &Tokens) {
        if let Some(redactor) = &self.redactor {
            redactor.add(&tokens.access_token);
            redactor.add(&tokens.refresh_token);
        }
    }

    /// Replaces the session's tokens, whose access token `used` is no good: with the stored
    /// tokens when another process has renewed them, else with refreshed ones, which are then
    /// stored. A profile signed out since (`harness logout` in another run) ends the session.
    async fn renew(&self, session: &mut Session, used: &str) -> Result<(), ProviderError> {
        let _lock = self.lock().await;
        match self.read_store().await {
            Ok(None) => return Err(self.signed_out()),
            Ok(now) => {
                self.take_in(session, now);
            }
            Err(e) => self.credentials.warn(format!(
                "cannot read the stored ChatGPT sign-in ({e}); renewing this session's"
            )),
        }
        if usable(&session.tokens, used) {
            return Ok(());
        }
        match self.oauth.refresh(&session.tokens).await {
            Ok(fresh) => {
                self.register(&fresh);
                session.tokens = fresh;
                self.save(session).await;
                Ok(())
            }
            Err(error) => {
                // Another process may have spent the refresh token without the lock (an older
                // harness), and stored what it got for it.
                if let Ok(now) = self.read_store().await
                    && self.take_in(session, now)
                    && usable(&session.tokens, used)
                {
                    return Ok(());
                }
                Err(refresh_error(error, &self.profile))
            }
        }
    }

    /// Takes the renewal lock, waiting for another process's renewal to end, which is told as a
    /// warning once it takes a while. Without the lock (it cannot be taken, or another process
    /// holds it too long), the renewal goes on, with a warning: the stored tokens are read again
    /// all the same.
    async fn lock(&self) -> Option<RenewalLock> {
        self.credentials
            .lock_renewal(
                PROVIDER,
                &self.profile,
                "renewing the ChatGPT sign-in",
                |note| self.credentials.warn(note),
            )
            .await
    }

    /// The error that ends a session whose profile has been signed out.
    fn signed_out(&self) -> ProviderError {
        ProviderError::Http {
            status: 401,
            body: format!(
                "signed out of ChatGPT (profile `{}`) since this session started; run `{}` to sign in again",
                self.profile,
                login_command(PROVIDER, &self.profile)
            ),
            retry_after: None,
        }
    }

    /// What the store holds for the profile now. The keychain may take a while to answer.
    async fn read_store(&self) -> Result<Option<Tokens>, CredentialError> {
        let (credentials, profile) = (self.credentials.clone(), self.profile.clone());
        tokio::task::spawn_blocking(move || stored(&credentials, &profile))
            .await
            .unwrap_or_else(|e| Err(CredentialError::Keychain(e.to_string())))
    }

    /// Takes in `now`, what the store holds: when someone else stored tokens for the session's
    /// account since the session last looked, the session uses them. Returns whether it does.
    fn take_in(&self, session: &mut Session, now: Option<Tokens>) -> bool {
        if now == session.seen {
            return false;
        }
        session.seen = now.clone();
        // Signed out elsewhere after a renewal found the profile signed in: nothing to take in.
        let Some(theirs) = now else {
            return false;
        };
        if theirs.account_id != session.tokens.account_id {
            if !session.told_of_other_account {
                session.told_of_other_account = true;
                self.credentials.warn(format!(
                    "the ChatGPT profile `{}` has been signed in to another ChatGPT account since this session started; the session goes on with its own account, and leaves that sign-in as it is",
                    self.profile
                ));
            }
            return false;
        }
        self.register(&theirs);
        session.tokens = theirs;
        true
    }

    /// Stores the session's tokens over what the store holds, when that is still the session's
    /// own sign-in: never over another account's, nor after a sign-out. A store that fails is a
    /// warning; the session keeps the tokens, and the next renewal stores its own.
    async fn save(&self, session: &mut Session) {
        let ours = session
            .seen
            .as_ref()
            .is_some_and(|seen| seen.account_id == session.tokens.account_id);
        if !ours {
            return;
        }
        let (credentials, profile) = (self.credentials.clone(), self.profile.clone());
        let json = session.tokens.to_json();
        let stored =
            tokio::task::spawn_blocking(move || credentials.set(PROVIDER, &profile, &json))
                .await
                .unwrap_or_else(|e| Err(CredentialError::Keychain(e.to_string())));
        match stored {
            Ok(_) => session.seen = Some(session.tokens.clone()),
            Err(e) => self.credentials.warn(format!(
                "the renewed ChatGPT sign-in could not be stored ({e}); this run goes on with it and tries again at its next renewal, but until then another run may have to sign in again with `{}`",
                login_command(PROVIDER, &self.profile)
            )),
        }
    }
}

/// The tokens stored under `profile`, if they are there and readable.
fn stored(credentials: &Credentials, profile: &str) -> Result<Option<Tokens>, CredentialError> {
    Ok(credentials
        .get(PROVIDER, profile)?
        .as_deref()
        .and_then(Tokens::from_json))
}

/// Whether `tokens` can be used in place of the access token `used`, which is no good.
fn usable(tokens: &Tokens, used: &str) -> bool {
    tokens.access_token != used && !expiring(tokens)
}

/// Whether the access token expires within [`REFRESH_MARGIN`]. A token without an expiry is used
/// until the server refuses it.
fn expiring(tokens: &Tokens) -> bool {
    tokens
        .expires_at()
        .is_some_and(|at| at <= now_unix() + REFRESH_MARGIN.as_secs())
}

/// A failed refresh as the provider's error. Only the sign-in server refusing the refresh token
/// (a 401, `invalid_grant`, or a refresh token that expired, was used, or was revoked) means
/// signing in again, and reads as the 401 it stands for; a server that cannot answer now (a 5xx,
/// a 429, no connection) can be retried, with the same tokens, as Codex does. Any other failure
/// keeps the tokens too, but retrying does not help: it says to sign in again should it persist.
fn refresh_error(error: OAuthError, profile: &str) -> ProviderError {
    const SIGNED_OUT: [&str; 4] = [
        "invalid_grant",
        "refresh_token_expired",
        "refresh_token_reused",
        "refresh_token_invalidated",
    ];
    let login = login_command(PROVIDER, profile);
    let persists = format!("; if this persists, run `{login}` to sign in again");
    match error {
        OAuthError::Network(message) => ProviderError::Network(format!(
            "cannot reach the sign-in server to renew the ChatGPT sign-in: {message}"
        )),
        OAuthError::Rejected { status, code, body }
            if status == 401
                || code.as_deref().is_some_and(|code| {
                    SIGNED_OUT.contains(&code.to_ascii_lowercase().as_str())
                }) =>
        {
            ProviderError::Http {
                status: 401,
                body: format!(
                    "the ChatGPT sign-in has ended (the sign-in server answered HTTP {status}: {body}); run `{login}` to sign in again"
                ),
                retry_after: None,
            }
        }
        OAuthError::Rejected { status, body, .. } => {
            // As `ProviderError::is_retryable` has it.
            let retryable = status == 429 || (500..600).contains(&status);
            ProviderError::Http {
                status,
                body: format!(
                    "the sign-in server could not renew the ChatGPT sign-in now: {body}{}",
                    if retryable { "" } else { persists.as_str() }
                ),
                retry_after: None,
            }
        }
        other => ProviderError::Protocol(format!(
            "cannot renew the ChatGPT sign-in: {other}{persists}"
        )),
    }
}
