//! Model provider adapters, local-server discovery, and model-id resolution.

pub mod anthropic_messages;
#[cfg(feature = "chatgpt-login")]
pub mod chatgpt;
pub mod credentials;
pub mod discovery;
mod http;
pub mod openai_chat;
pub mod openai_responses;
pub mod profiles;
pub mod registry;
mod sse;
pub mod window;
