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

/// How long a response may send nothing, once its first data came, before the server counts as
/// no longer responding.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a hosted provider may take, from the request, to send its reply's first data.
pub const FIRST_DATA_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a local server may take: it may first load the model, and read a long prompt on a
/// CPU.
pub const LOCAL_FIRST_DATA_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long a response may keep the reader waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Waits {
    /// For the first data, from the request on, headers included.
    first: Duration,
    /// Between pieces of data after that.
    idle: Duration,
}

impl Waits {
    /// The waits for a local server (`local`, as the model's profile says) or a hosted provider.
    fn for_server(local: bool) -> Waits {
        Waits {
            first: if local {
                LOCAL_FIRST_DATA_TIMEOUT
            } else {
                FIRST_DATA_TIMEOUT
            },
            idle: IDLE_TIMEOUT,
        }
    }
}

/// Awaits `response`, then streams its events through `parser`. An error status ends the stream
/// with [`ProviderError::Http`]; a stream that ends before the reply finished, or that keeps the
/// reader waiting too long, with [`ProviderError::Network`]: one that sends no data within
/// [`FIRST_DATA_TIMEOUT`] of the request ([`LOCAL_FIRST_DATA_TIMEOUT`] for a `local` server), or
/// nothing for [`IDLE_TIMEOUT`] after that. Any data counts, a keep-alive comment included.
pub fn events<P: EventParser>(
    response: impl Future<Output = Result<reqwest::Response, ProviderError>> + Send + 'static,
    parser: P,
    local: bool,
) -> ProviderStream {
    events_within(response, parser, Waits::for_server(local))
}

/// Why reading a response's body stopped.
enum Read {
    /// No data came within the first wait.
    NoStart,
    /// Nothing arrived for the idle limit.
    Idle,
    Failed(reqwest::Error),
}

/// [`events`], with these `waits`.
fn events_within<P: EventParser>(
    response: impl Future<Output = Result<reqwest::Response, ProviderError>> + Send + 'static,
    mut parser: P,
    waits: Waits,
) -> ProviderStream {
    let no_start = move || {
        ProviderError::Network(format!(
            "the server did not start its reply within {}",
            duration(waits.first)
        ))
    };
    // The if/else keeps `response` used within one branch: `http_error` takes it by value.
    Box::pin(async_stream::try_stream! {
        // The first wait runs from the request on: the headers can be what is slow.
        let first = tokio::time::Instant::now() + waits.first;
        let response = tokio::time::timeout_at(first, response)
            .await
            .map_err(|_| no_start())??;
        if response.status().is_success() {
            let mut body = response.bytes_stream();
            // Each wait for data is limited, not the whole reply: until the first data, to what is
            // left of the first wait; after it, to the idle limit.
            let chunks = async_stream::stream! {
                let mut started = false;
                loop {
                    let next = if started {
                        tokio::time::timeout(waits.idle, body.next()).await
                    } else {
                        tokio::time::timeout_at(first, body.next()).await
                    };
                    match next {
                        Ok(Some(Ok(chunk))) => {
                            started = true;
                            yield Ok(chunk);
                        }
                        Ok(Some(Err(e))) => {
                            yield Err(Read::Failed(e));
                            break;
                        }
                        Ok(None) => break,
                        Err(_) => {
                            yield Err(if started { Read::Idle } else { Read::NoStart });
                            break;
                        }
                    }
                }
            };
            let mut events = Box::pin(chunks).eventsource();
            while let Some(event) = events.next().await {
                let event = event.map_err(|e| match e {
                    EventStreamError::Transport(Read::NoStart) => no_start(),
                    EventStreamError::Transport(Read::Idle) => ProviderError::Network(format!(
                        "the server stopped responding: nothing arrived for {}",
                        duration(waits.idle)
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

/// `duration` as a person reads it: in whole minutes from ten minutes on, in seconds from one
/// second, and in milliseconds below that.
fn duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 600 && secs.is_multiple_of(60) {
        format!("{} min", secs / 60)
    } else if secs >= 1 {
        format!("{secs} s")
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

    async fn collect(url: &str, waits: Waits) -> Vec<Result<ProviderEvent, ProviderError>> {
        let request = reqwest::Client::new().get(url);
        events_within(send(request), Lines::default(), waits)
            .collect()
            .await
    }

    fn waits(first: Duration, idle: Duration) -> Waits {
        Waits { first, idle }
    }

    fn network_message(events: &[Result<ProviderEvent, ProviderError>]) -> String {
        match events.last() {
            Some(Err(error @ ProviderError::Network(message))) => {
                assert!(error.is_retryable());
                message.clone()
            }
            other => panic!("{other:?}"),
        }
    }

    // Ruling on review A M7: a hosted provider starts its reply within 300 s, a local server
    // (one that may load the model and read a long prompt on a CPU first) within 30 minutes;
    // after that, 300 s without data means the server stopped.
    #[test]
    fn a_local_server_gets_longer_to_start_its_reply() {
        let hosted = Waits::for_server(false);
        let local = Waits::for_server(true);
        assert_eq!(hosted.first, Duration::from_secs(300));
        assert_eq!(local.first, Duration::from_secs(30 * 60));
        assert_eq!(hosted.idle, Duration::from_secs(300));
        assert_eq!(local.idle, Duration::from_secs(300));
    }

    // Review A M7: a server that stops sending without closing the connection must not hang a
    // headless run: the stream ends with an error that can be retried.
    #[tokio::test]
    async fn a_server_that_stops_sending_ends_the_stream_as_a_retryable_error() {
        let url = serve(ms(0), vec![(ms(0), "one")], false).await;
        let started = Instant::now();
        let events = collect(&url, waits(ms(5_000), ms(300))).await;
        assert_eq!(events[0], Ok(ProviderEvent::TextDelta("one".into())));
        let message = network_message(&events);
        assert!(
            message.contains("the server stopped responding: nothing arrived for 300 ms"),
            "{message}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    // The limit is on silence between pieces, not on the whole reply.
    #[tokio::test]
    async fn a_slow_but_steady_stream_is_not_cut() {
        let steady = vec![
            (ms(0), "one"),
            (ms(200), "two"),
            (ms(200), "three"),
            (ms(200), "end"),
        ];
        let url = serve(ms(0), steady, false).await;
        let events = collect(&url, waits(ms(2_000), ms(500))).await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert_eq!(events.len(), 4, "{events:?}");
    }

    // Before the first data, only the first wait counts, from the request on, whether the server
    // pauses before its headers (Ollama loading the model) or after them (llama.cpp reading the
    // prompt).
    #[tokio::test]
    async fn the_first_data_has_a_wait_of_its_own() {
        for (before_headers, after_headers) in [(ms(700), ms(0)), (ms(0), ms(700))] {
            let reply = || vec![(after_headers, "one"), (ms(0), "end")];
            // Longer than the idle limit, within the first wait: the reply comes.
            let url = serve(before_headers, reply(), false).await;
            let events = collect(&url, waits(ms(2_000), ms(300))).await;
            assert!(events.iter().all(Result::is_ok), "{events:?}");
            // Longer than the first wait: it ends, saying so.
            let url = serve(before_headers, reply(), false).await;
            let started = Instant::now();
            let events = collect(&url, waits(ms(300), ms(5_000))).await;
            let message = network_message(&events);
            assert!(
                message.contains("the server did not start its reply within 300 ms"),
                "{message}"
            );
            assert!(started.elapsed() < ms(650), "{:?}", started.elapsed());
        }
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
        let events = collect(
            &format!("{url}/v1?key=SECRETQ"),
            waits(ms(2_000), ms(2_000)),
        )
        .await;
        let error = events.last().unwrap().clone().unwrap_err();
        assert!(matches!(error, ProviderError::Network(_)), "{error:?}");
        assert!(!error.to_string().contains("SECRETQ"), "{error}");
    }
}
