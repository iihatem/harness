//! ChatGPT sign-in and the `chatgpt` provider (feature `chatgpt-login`). The OAuth flows follow
//! OpenAI's open-source Codex CLI (github.com/openai/codex, Apache-2.0, `codex-rs/login`), use
//! its public client, and identify to OpenAI as it (the Codex CLI's `originator` and scopes, not
//! a `harness`-specific identity). This is not an OpenAI endorsement of harness.

pub mod auth;
pub mod oauth;
pub mod usage;
