//! What the adapters share: sending a request, turning an error response into a
//! [`ProviderError`], and reading a success response's server-sent events through a parser.

use std::{future::Future, time::Duration};

use eventsource_stream::Eventsource;
use futures::StreamExt;
use harness_core::provider::{ProviderError, ProviderEvent, ProviderStream};

/// Turns the `data:` payloads of one response's server-sent events into [`ProviderEvent`]s.
pub trait EventParser: Send + 'static {
    /// The events one payload yields.
    fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError>;
    /// What is still buffered, then `Finished`. Calling it again yields nothing.
    fn finish(&mut self) -> Vec<ProviderEvent>;
    /// Whether the reply is complete, so the rest of the stream need not be read.
    fn is_done(&self) -> bool;
    /// Whether the stream may end here without the reply having been cut off.
    fn may_end(&self) -> bool {
        self.is_done()
    }
}

/// Awaits `response`, then streams its events through `parser`. An error status ends the stream
/// with [`ProviderError::Http`]; a stream that ends before the reply finished, with
/// [`ProviderError::Network`].
pub fn events<P: EventParser>(
    response: impl Future<Output = Result<reqwest::Response, ProviderError>> + Send + 'static,
    mut parser: P,
) -> ProviderStream {
    // The if/else keeps `response` used within one branch: `http_error` takes it by value.
    Box::pin(async_stream::try_stream! {
        let response = response.await?;
        if response.status().is_success() {
            let mut events = response.bytes_stream().eventsource();
            while let Some(event) = events.next().await {
                let event = event.map_err(|e| ProviderError::Network(e.to_string()))?;
                for item in parser.push(&event.data)? {
                    yield item;
                }
                if parser.is_done() {
                    break;
                }
            }
            // The connection closed mid-reply: that must not pass for a normal completion.
            if !parser.may_end() {
                Err::<(), ProviderError>(ProviderError::Network(
                    "stream ended before the response finished".into(),
                ))?;
            }
            for item in parser.finish() {
                yield item;
            }
        } else {
            // `?` on an `Err` ends the stream with this error.
            Err::<(), ProviderError>(http_error(response).await)?;
        }
    })
}

/// Sends `request`, mapping a failure to connect or send to [`ProviderError::Network`].
pub async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, ProviderError> {
    request
        .send()
        .await
        .map_err(|e| ProviderError::Network(e.to_string()))
}

/// The error an unsuccessful response stands for: its status, body and `Retry-After` in seconds.
pub async fn http_error(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = response.text().await.unwrap_or_default();
    ProviderError::Http {
        status,
        body,
        retry_after,
    }
}
