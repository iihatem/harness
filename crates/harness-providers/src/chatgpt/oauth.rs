//! ChatGPT sign-in: OAuth 2.0 authorization code with PKCE, through the browser and a callback
//! on `127.0.0.1`, or through a device code; and refreshing the access token. The endpoints,
//! client id, scopes, originator and callback ports are those of OpenAI's Codex CLI (openai/codex,
//! `codex-rs/login/src/server.rs`, `device_code_auth.rs` and `auth/manager.rs`): harness signs in
//! with the Codex CLI's OAuth client and identifies to OpenAI as it, not as a distinct client.

use std::{
    collections::HashMap,
    io::Read,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// OpenAI's authorization server.
pub const ISSUER: &str = "https://auth.openai.com";
/// The public OAuth client of OpenAI's Codex CLI.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// What harness asks for: who the user is, a refresh token, and the connector scopes the Codex
/// CLI also asks for (unused by harness, but part of identifying as it).
pub const SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
/// The callback ports registered for that client: the first, then the fallback.
pub const CALLBACK_PORTS: [u16; 2] = [1455, 1457];
/// How harness names itself to the authorization server and to ChatGPT's backend: the Codex
/// CLI's own originator, not harness's. harness signs in with the Codex CLI's OAuth client and
/// identifies to OpenAI as it, rather than presenting itself as a distinct client.
pub const ORIGINATOR: &str = "codex_cli_rs";
/// What ChatGPT may append to the `state` it sends back, followed by a value (Codex strips
/// `.onboarding_entrypoint=life_sciences`).
const ONBOARDING_SUFFIX: &str = ".onboarding_entrypoint=";
/// How long a device code stays valid.
pub const DEVICE_CODE_WAIT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("cannot reach the sign-in server: {0}")]
    Network(String),
    #[error("the sign-in server refused (HTTP {status}): {body}")]
    Rejected {
        status: u16,
        /// The OAuth error code: `error` when it is a string, else `error.code` or `code`.
        code: Option<String>,
        body: String,
    },
    #[error("the sign-in server answered something unexpected: {0}")]
    Invalid(String),
    #[error("sign-in was refused: {0}")]
    Denied(String),
    #[error("sign-in timed out")]
    TimedOut,
    #[error("the sign-in server's address `{0}` is not an http(s) URL")]
    BadIssuer(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// A PKCE verifier and its S256 challenge.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// A fresh verifier: 64 random bytes in base64url.
    pub fn generate() -> std::io::Result<Pkce> {
        Ok(Pkce::from_verifier(
            &URL_SAFE_NO_PAD.encode(random_bytes::<64>()?),
        ))
    }

    pub fn from_verifier(verifier: &str) -> Pkce {
        Pkce {
            verifier: verifier.to_string(),
            challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        }
    }
}

/// A random `state` for one sign-in.
pub fn random_state() -> std::io::Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes::<32>()?))
}

fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// What a sign-in leaves: the tokens, and the ChatGPT account and email from the ID token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Sent as `ChatGPT-Account-ID` with every request.
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// Leaves the tokens out.
impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .finish()
    }
}

impl Tokens {
    /// When the access token expires, in seconds since the Unix epoch, from its `exp` claim.
    pub fn expires_at(&self) -> Option<u64> {
        claims(&self.access_token)?["exp"].as_u64()
    }

    /// How the tokens are stored.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("tokens serialize")
    }

    /// Stored tokens, or `None` for something that is not.
    pub fn from_json(text: &str) -> Option<Tokens> {
        serde_json::from_str(text).ok()
    }

    /// Takes the account and email from `id_token`, when there is one.
    fn with_identity(mut self, id_token: Option<&str>) -> Tokens {
        if let Some(claims) = id_token.and_then(claims) {
            if let Some(account) =
                claims["https://api.openai.com/auth"]["chatgpt_account_id"].as_str()
            {
                self.account_id = Some(account.to_string());
            }
            let email = claims["email"]
                .as_str()
                .or_else(|| claims["https://api.openai.com/profile"]["email"].as_str());
            if let Some(email) = email {
                self.email = Some(email.to_string());
            }
        }
        self
    }
}

/// The claims of a JWT, without checking its signature: harness only reads what the server it
/// just talked to sent.
fn claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A device code the user enters at `verification_url`.
#[derive(Clone)]
pub struct DeviceCode {
    pub verification_url: String,
    pub user_code: String,
    device_auth_id: String,
    interval: Duration,
}

/// Leaves the device auth id out: with the user code, it fetches the tokens.
impl std::fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceCode")
            .field("verification_url", &self.verification_url)
            .field("user_code", &self.user_code)
            .field("device_auth_id", &"[redacted]")
            .field("interval", &self.interval)
            .finish()
    }
}

/// The authorization server's endpoints, for one client.
pub struct OAuth {
    client: reqwest::Client,
    issuer: String,
    client_id: String,
}

impl OAuth {
    /// The endpoints of the authorization server at `issuer`, an http(s) URL. Every request
    /// carries the Codex CLI's `originator`, as Codex's own client does, and harness's
    /// User-Agent.
    pub fn new(issuer: &str) -> Result<OAuth, OAuthError> {
        let issuer = issuer.trim_end_matches('/');
        let valid = reqwest::Url::parse(&format!("{issuer}/oauth/authorize"))
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host());
        if !valid {
            return Err(OAuthError::BadIssuer(issuer.to_string()));
        }
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "originator",
            reqwest::header::HeaderValue::from_static(ORIGINATOR),
        );
        Ok(OAuth {
            client: crate::http::client()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("harness/", env!("CARGO_PKG_VERSION")))
                .default_headers(headers)
                .build()
                .expect("an HTTP client builds"),
            issuer: issuer.to_string(),
            client_id: CLIENT_ID.to_string(),
        })
    }

    /// Where the browser goes to sign in.
    pub fn authorize_url(&self, redirect_uri: &str, pkce: &Pkce, state: &str) -> String {
        let mut url = reqwest::Url::parse(&format!("{}/oauth/authorize", self.issuer))
            .expect("checked by OAuth::new");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", ORIGINATOR);
        url.to_string()
    }

    /// Exchanges an authorization code for tokens.
    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<Tokens, OAuthError> {
        let body = form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &self.client_id),
            ("code_verifier", verifier),
        ]);
        let response = self
            .client
            .post(format!("{}/oauth/token", self.issuer))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await;
        let value = read(response).await?;
        let tokens = Tokens {
            access_token: string(&value, "access_token")?,
            refresh_token: string(&value, "refresh_token")?,
            account_id: None,
            email: None,
        };
        Ok(tokens.with_identity(value["id_token"].as_str()))
    }

    /// Asks for a device code.
    pub async fn request_device_code(&self) -> Result<DeviceCode, OAuthError> {
        let response = self
            .client
            .post(format!("{}/api/accounts/deviceauth/usercode", self.issuer))
            .json(&json!({"client_id": self.client_id}))
            .send()
            .await;
        let value = read(response).await?;
        let user_code = value["user_code"]
            .as_str()
            .or_else(|| value["usercode"].as_str())
            .ok_or_else(|| OAuthError::Invalid("no user code".into()))?;
        // Sent as a string, "5"; a number is taken too.
        let interval = value["interval"]
            .as_u64()
            .or_else(|| value["interval"].as_str()?.trim().parse().ok())
            .unwrap_or(5);
        Ok(DeviceCode {
            verification_url: format!("{}/codex/device", self.issuer),
            user_code: user_code.to_string(),
            device_auth_id: string(&value, "device_auth_id")?,
            interval: Duration::from_secs(interval),
        })
    }

    /// Waits, up to `max_wait`, for the user to enter the device code, then exchanges the code
    /// the server issues for tokens. A server that is busy (429) or failing (5xx) meanwhile is
    /// waited out, a little longer each time it is, until the code expires.
    pub async fn poll_device_code(
        &self,
        device: &DeviceCode,
        max_wait: Duration,
    ) -> Result<Tokens, OAuthError> {
        let started = Instant::now();
        let interval = device.interval.max(Duration::from_millis(100));
        let mut pause = interval;
        loop {
            let response = self
                .client
                .post(format!("{}/api/accounts/deviceauth/token", self.issuer))
                .json(&json!({"device_auth_id": device.device_auth_id, "user_code": device.user_code}))
                .send()
                .await
                .map_err(|e| OAuthError::Network(crate::http::describe(&e)))?;
            let status = response.status().as_u16();
            // Not approved yet (403, 404), or the server cannot say now (429, 5xx).
            let not_yet = matches!(status, 403 | 404);
            let busy = status == 429 || (500..600).contains(&status);
            if not_yet || busy {
                pause = if busy {
                    (pause * 2).min(Duration::from_secs(30))
                } else {
                    interval
                };
                let waited = started.elapsed();
                if waited >= max_wait {
                    return Err(OAuthError::TimedOut);
                }
                tokio::time::sleep(pause.min(max_wait - waited)).await;
                continue;
            }
            let value = read(Ok(response)).await?;
            let redirect_uri = format!("{}/deviceauth/callback", self.issuer);
            return self
                .exchange_code(
                    &string(&value, "authorization_code")?,
                    &redirect_uri,
                    &string(&value, "code_verifier")?,
                )
                .await;
        }
    }

    /// New tokens for `tokens`. What the server does not replace (the refresh token, often) is
    /// kept.
    pub async fn refresh(&self, tokens: &Tokens) -> Result<Tokens, OAuthError> {
        let response = self
            .client
            .post(format!("{}/oauth/token", self.issuer))
            .json(&json!({
                "client_id": self.client_id,
                "grant_type": "refresh_token",
                "refresh_token": tokens.refresh_token,
            }))
            .send()
            .await;
        let value = read(response).await?;
        let refreshed = Tokens {
            access_token: string(&value, "access_token")?,
            refresh_token: value["refresh_token"]
                .as_str()
                .unwrap_or(&tokens.refresh_token)
                .to_string(),
            ..tokens.clone()
        };
        Ok(refreshed.with_identity(value["id_token"].as_str()))
    }
}

/// `pairs` as an `application/x-www-form-urlencoded` body.
fn form(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://form.invalid/").expect("a static URL");
    url.query_pairs_mut().extend_pairs(pairs);
    url.query().unwrap_or_default().to_string()
}

/// The JSON of a successful response.
async fn read(response: reqwest::Result<reqwest::Response>) -> Result<Value, OAuthError> {
    let response = response.map_err(|e| OAuthError::Network(crate::http::describe(&e)))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| OAuthError::Network(crate::http::describe(&e)))?;
    if !status.is_success() {
        let value: Value = serde_json::from_str(&text).unwrap_or_default();
        let code = [&value["error"], &value["error"]["code"], &value["code"]]
            .into_iter()
            .find_map(|v| v.as_str().filter(|c| !c.is_empty()))
            .map(String::from);
        return Err(OAuthError::Rejected {
            status: status.as_u16(),
            code,
            body: text.chars().take(500).collect(),
        });
    }
    serde_json::from_str(&text).map_err(|e| OAuthError::Invalid(e.to_string()))
}

fn string(value: &Value, key: &str) -> Result<String, OAuthError> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| OAuthError::Invalid(format!("no {key}")))
}

/// The browser's way back: a small HTTP server on `127.0.0.1` that waits for the redirect from
/// the authorization server.
pub struct CallbackServer {
    listener: TcpListener,
    port: u16,
}

impl CallbackServer {
    /// Listens at the first of `ports` that is free (`0`: any). When none is, the error names
    /// them and suggests the device flow, which needs no port.
    pub async fn bind(ports: &[u16]) -> std::io::Result<CallbackServer> {
        let mut last = None;
        for &port in ports {
            match TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => {
                    let port = listener.local_addr()?.port();
                    return Ok(CallbackServer { listener, port });
                }
                Err(e) => last = Some(e),
            }
        }
        let Some(last) = last else {
            return Err(std::io::Error::other("no port to listen on"));
        };
        let names: Vec<String> = ports.iter().map(u16::to_string).collect();
        Err(std::io::Error::new(
            last.kind(),
            format!(
                "cannot listen for the browser's return on 127.0.0.1, port {} ({last}); sign in with a device code instead: add --device",
                names.join(" or ")
            ),
        ))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The redirect URI the authorization server sends the browser back to.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/auth/callback", self.port)
    }

    /// Waits for the redirect that carries `state`, and returns its code. A request with another
    /// state, or for another path, is answered and ignored.
    pub async fn wait_for_code(&self, state: &str) -> Result<String, OAuthError> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            let Some(target) = request_target(&mut stream).await else {
                respond(&mut stream, 400, "Bad request.").await;
                continue;
            };
            let url = match reqwest::Url::parse(&format!("http://127.0.0.1{target}")) {
                Ok(url) if url.path() == "/auth/callback" => url,
                _ => {
                    respond(&mut stream, 404, "Not found.").await;
                    continue;
                }
            };
            let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
            // ChatGPT may append onboarding metadata to the state; Codex strips it too.
            let returned = query.get("state").map(|s| {
                s.split_once(ONBOARDING_SUFFIX)
                    .map_or(s.as_str(), |(s, _)| s)
            });
            if returned != Some(state) {
                respond(
                    &mut stream,
                    400,
                    "This sign-in is not the one harness started. Go back to the terminal.",
                )
                .await;
                continue;
            }
            if let Some(error) = query.get("error") {
                let reason = query
                    .get("error_description")
                    .filter(|d| !d.is_empty())
                    .unwrap_or(error)
                    .clone();
                let page = format!("Sign-in failed: {reason}. Go back to the terminal.");
                respond(&mut stream, 200, &page).await;
                return Err(OAuthError::Denied(reason));
            }
            let Some(code) = query.get("code").filter(|c| !c.is_empty()) else {
                respond(&mut stream, 400, "The sign-in carried no code.").await;
                return Err(OAuthError::Invalid("the callback carried no code".into()));
            };
            respond(
                &mut stream,
                200,
                "harness is signed in to ChatGPT. You can close this tab.",
            )
            .await;
            return Ok(code.clone());
        }
    }
}

/// The target of an HTTP `GET` request, from its request line. A client that sends nothing for
/// five seconds is dropped, so it cannot hold up the real callback.
async fn request_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    let read = async {
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
            let n = stream.read(&mut buf).await.ok()?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        Some(())
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .ok()??;
    let head = String::from_utf8_lossy(&head);
    let mut words = head.lines().next()?.split_whitespace();
    match (words.next(), words.next()) {
        (Some("GET"), Some(target)) if target.starts_with('/') => Some(target.to_string()),
        _ => None,
    }
}

/// A plain-text page: nothing from the request is ever rendered as HTML.
async fn respond(stream: &mut TcpStream, status: u16, text: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}
