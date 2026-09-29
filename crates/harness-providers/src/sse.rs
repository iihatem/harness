//! What the adapters share: sending a request, turning an error response into a
//! [`ProviderError`], and reading a success response's server-sent events through a parser.

use std::{future::Future, time::Duration};

use eventsource_stream::{EventStreamError, Eventsource};
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

/// How long a response may send nothing before the server counts as no longer responding. The
/// wait for the response to start is not counted: a local server may first load the model.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Awaits `response`, then streams its events through `parser`. An error status ends the stream
/// with [`ProviderError::Http`]; a stream that ends before the reply finished, or that sends
/// nothing for [`IDLE_TIMEOUT`], with [`ProviderError::Network`].
pub fn events<P: EventParser>(
    response: impl Future<Output = Result<reqwest::Response, ProviderError>> + Send + 'static,
    parser: P,
) -> ProviderStream {
    events_within(response, parser, IDLE_TIMEOUT)
}

/// Why reading a response's body stopped.
enum Read {
    /// Nothing arrived for the idle limit.
    Idle,
    Failed(reqwest::Error),
}

/// [`events`], with `idle` as the idle limit.
fn events_within<P: EventParser>(
    response: impl Future<Output = Result<reqwest::Response, ProviderError>> + Send + 'static,
    mut parser: P,
    idle: Duration,
) -> ProviderStream {
    // The if/else keeps `response` used within one branch: `http_error` takes it by value.
    Box::pin(async_stream::try_stream! {
        let response = response.await?;
        if response.status().is_success() {
            let mut body = response.bytes_stream();
            // Each wait for bytes is limited, not the whole reply.
            let chunks = async_stream::stream! {
                loop {
                    match tokio::time::timeout(idle, body.next()).await {
                        Ok(Some(Ok(chunk))) => yield Ok(chunk),
                        Ok(Some(Err(e))) => {
                            yield Err(Read::Failed(e));
                            break;
                        }
                        Ok(None) => break,
                        Err(_) => {
                            yield Err(Read::Idle);
                            break;
                        }
                    }
                }
            };
            let mut events = Box::pin(chunks).eventsource();
            while let Some(event) = events.next().await {
                let event = event.map_err(|e| match e {
                    EventStreamError::Transport(Read::Idle) => ProviderError::Network(format!(
                        "the server stopped responding: nothing arrived for {}",
                        duration(idle)
                    )),
                    EventStreamError::Transport(Read::Failed(e)) => network_error(e),
                    EventStreamError::Utf8(e) => ProviderError::Network(e.to_string()),
                    EventStreamError::Parser(e) => ProviderError::Network(e.to_string()),
                })?;
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

/// `duration` as a person reads it: in seconds, or in milliseconds below one.
fn duration(duration: Duration) -> String {
    if duration >= Duration::from_secs(1) {
        format!("{} s", duration.as_secs())
    } else {
        format!("{} ms", duration.as_millis())
    }
}

/// Sends `request`, mapping a failure to connect or send to [`ProviderError::Network`].
pub async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, ProviderError> {
    request.send().await.map_err(network_error)
}

/// A failure on the network, described with the URL's host and path only: its query, fragment and
/// credentials are left out, since a key can be kept there.
pub fn network_error(mut error: reqwest::Error) -> ProviderError {
    if let Some(url) = error.url_mut() {
        url.set_query(None);
        url.set_fragment(None);
        let _ = url.set_username("");
        let _ = url.set_password(None);
    }
    ProviderError::Network(error.to_string())
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use futures::StreamExt;
    use harness_core::provider::FinishReason;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// Yields each payload as text; done at `end`.
    #[derive(Default)]
    struct Lines {
        done: bool,
        finished: bool,
    }

    impl EventParser for Lines {
        fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
            if data == "end" {
                self.done = true;
                return Ok(Vec::new());
            }
            Ok(vec![ProviderEvent::TextDelta(data.to_string())])
        }

        fn finish(&mut self) -> Vec<ProviderEvent> {
            if std::mem::replace(&mut self.finished, true) {
                return Vec::new();
            }
            vec![ProviderEvent::Finished(FinishReason::Stop)]
        }

        fn is_done(&self) -> bool {
            self.done
        }
    }

    /// Serves one streamed response on a local port: after `before_headers`, the headers, then
    /// each event after its delay; then it keeps the connection open, sending nothing, or, when
    /// `cut` is set, closes it in the middle of a chunk. Returns the server's URL.
    async fn serve(
        before_headers: Duration,
        events: Vec<(Duration, &'static str)>,
        cut: bool,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await;
            tokio::time::sleep(before_headers).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
            socket.write_all(head.as_bytes()).await.unwrap();
            for (delay, data) in events {
                tokio::time::sleep(delay).await;
                let event = format!("data: {data}\n\n");
                let chunk = format!("{:x}\r\n{event}\r\n", event.len());
                socket.write_all(chunk.as_bytes()).await.unwrap();
            }
            if cut {
                socket.write_all(b"40\r\ndata: partial").await.unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        url
    }

    async fn collect(url: &str, idle: Duration) -> Vec<Result<ProviderEvent, ProviderError>> {
        let request = reqwest::Client::new().get(url);
        events_within(send(request), Lines::default(), idle)
            .collect()
            .await
    }

    // Review A M7: a server that stops sending without closing the connection must not hang a
    // headless run: the stream ends with an error that can be retried.
    #[tokio::test]
    async fn a_server_that_stops_sending_ends_the_stream_as_a_retryable_error() {
        let url = serve(ms(0), vec![(ms(0), "one")], false).await;
        let started = Instant::now();
        let events = collect(&url, ms(300)).await;
        assert_eq!(events[0], Ok(ProviderEvent::TextDelta("one".into())));
        let error = events.last().unwrap().clone().unwrap_err();
        assert!(
            matches!(&error, ProviderError::Network(m) if m.contains("the server stopped responding")),
            "{error:?}"
        );
        assert!(error.is_retryable());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    // The limit is on silence, not on the whole reply; and the wait for the response to start
    // (Ollama loading a model, say) is not counted.
    #[tokio::test]
    async fn a_slow_but_steady_stream_and_a_slow_start_are_not_cut() {
        let steady = vec![
            (ms(0), "one"),
            (ms(200), "two"),
            (ms(200), "three"),
            (ms(200), "end"),
        ];
        let url = serve(ms(700), steady, false).await;
        let events = collect(&url, ms(500)).await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert_eq!(events.len(), 4, "{events:?}");
    }

    // Review A M8: a key kept in the URL's query never appears in a network error.
    #[tokio::test]
    async fn network_errors_leave_out_the_urls_query() {
        let port = {
            let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            closed.local_addr().unwrap().port()
        };
        let url = format!("http://user:pw@127.0.0.1:{port}/v1/messages?key=SECRETQ#frag");
        let error = send(reqwest::Client::new().get(&url)).await.unwrap_err();
        let text = error.to_string();
        assert!(matches!(error, ProviderError::Network(_)), "{error:?}");
        assert!(
            text.contains(&format!("127.0.0.1:{port}/v1/messages")),
            "{text}"
        );
        for hidden in ["SECRETQ", "frag", "pw"] {
            assert!(!text.contains(hidden), "{text}");
        }

        // Also when the connection breaks in the middle of the reply.
        let url = serve(ms(0), vec![(ms(0), "one")], true).await;
        let events = collect(&format!("{url}/v1?key=SECRETQ"), ms(2_000)).await;
        let error = events.last().unwrap().clone().unwrap_err();
        assert!(matches!(error, ProviderError::Network(_)), "{error:?}");
        assert!(!error.to_string().contains("SECRETQ"), "{error}");
    }
}
