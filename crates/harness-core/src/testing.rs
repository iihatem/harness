//! Test doubles. Small and dependency-free, so they are always compiled and usable from any test crate.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use futures::StreamExt;

use crate::{
    message::{ChatRequest, ToolCall},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderStream},
};

/// One scripted model response.
pub enum Script {
    /// Yield these items, then end the stream.
    Reply(Vec<Result<ProviderEvent, ProviderError>>),
    /// Yield these events, then never finish (for interrupt tests).
    Hang(Vec<ProviderEvent>),
}

impl Script {
    pub fn text(text: &str) -> Script {
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta(text.to_string())),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ])
    }

    pub fn tool_call(id: &str, name: &str, args: serde_json::Value) -> Script {
        Self::raw_tool_call(id, name, &args.to_string())
    }

    pub fn raw_tool_call(id: &str, name: &str, arguments: &str) -> Script {
        Script::Reply(vec![
            Ok(ProviderEvent::ToolCall(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
            })),
            Ok(ProviderEvent::Finished(FinishReason::ToolCalls)),
        ])
    }

    pub fn error(error: ProviderError) -> Script {
        Script::Reply(vec![Err(error)])
    }
}

/// A provider that replays a script, one entry per request, and records every request.
pub struct MockProvider {
    script: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl MockProvider {
    pub fn new(script: Vec<Script>) -> Arc<MockProvider> {
        Arc::new(MockProvider {
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Provider for MockProvider {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        self.requests.lock().expect("requests lock").push(request);
        match self.script.lock().expect("script lock").pop_front() {
            Some(Script::Reply(items)) => Box::pin(futures::stream::iter(items)),
            Some(Script::Hang(before)) => Box::pin(
                futures::stream::iter(before.into_iter().map(Ok)).chain(futures::stream::pending()),
            ),
            None => Box::pin(futures::stream::iter(vec![Err(ProviderError::Protocol(
                "mock script exhausted".into(),
            ))])),
        }
    }
}
