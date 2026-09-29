use serde::{Deserialize, Serialize};

/// A tool invocation requested by the model. `arguments` is the raw JSON text the model produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A provider-neutral conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: String,
    },
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
        model: String,
    },
    Tool {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

/// A tool definition sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Token accounting reported by a provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

/// Settings a model profile gives each request. `None` leaves the provider's default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestOptions {
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    /// For models that reason: `minimal`, `low`, `medium` or `high`, as the provider names it.
    pub reasoning_effort: Option<String>,
}

/// Everything a provider needs for one model call.
#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    /// Model name as the provider knows it (without the `<provider>/` prefix).
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub options: RequestOptions,
    /// Tokens the model's context window has left after this request's input, as the agent
    /// estimates them; `None` when it does not know the window. An adapter that must send an
    /// output limit keeps it within this, since input and output share the window.
    pub output_room: Option<u64>,
}
