//! ChatGPT sign-in and the `chatgpt` provider (feature `chatgpt-login`). The OAuth flows follow
//! OpenAI's open-source Codex CLI (github.com/openai/codex, Apache-2.0, `codex-rs/login`) and use
//! its public client.

pub mod auth;
pub mod oauth;
