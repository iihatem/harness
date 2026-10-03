//! JSON-RPC messages framed with `Content-Length` headers, as language servers speak them.

use std::io;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// The longest message accepted: a server that sends more is not a language server we can use.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// The longest header line accepted.
const MAX_HEADER: usize = 8 * 1024;

/// The bytes to write for `message`: the header and the JSON.
pub fn encode(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut bytes = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

/// The next message, or `None` at the end of the stream. Headers other than `Content-Length` are
/// ignored. A body that is not JSON is skipped (the length was readable, so the stream is in step);
/// a header that is too long or has no usable length, or a body that is cut short, is an error.
pub async fn read<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Value>> {
    loop {
        let mut length = None;
        loop {
            let mut line = String::new();
            let read = (&mut *reader)
                .take(MAX_HEADER as u64)
                .read_line(&mut line)
                .await?;
            if read == 0 {
                return Ok(None);
            }
            if !line.ends_with('\n') && read >= MAX_HEADER {
                return Err(io::Error::other("a header line is too long"));
            }
            let line = line.trim_end();
            if line.is_empty() {
                if length.is_some() {
                    break;
                }
                continue;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
            }
        }
        let length = length.unwrap_or(0);
        if length > MAX_BODY {
            return Err(io::Error::other("a message is too long"));
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).await?;
        if let Ok(message) = serde_json::from_slice(&body) {
            return Ok(Some(message));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[tokio::test]
    async fn a_message_survives_the_round_trip_and_the_stream_ends_cleanly() {
        let message = json!({"jsonrpc": "2.0", "method": "x", "params": {"s": "é\n"}});
        let mut bytes = encode(&message);
        bytes.extend(encode(&json!({"id": 2})));
        let mut reader = tokio::io::BufReader::new(&bytes[..]);
        assert_eq!(read(&mut reader).await.unwrap(), Some(message));
        assert_eq!(read(&mut reader).await.unwrap(), Some(json!({"id": 2})));
        assert_eq!(read(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn other_headers_are_ignored_and_the_name_is_case_insensitive() {
        let raw = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: 2\r\n\r\n{}";
        let mut reader = tokio::io::BufReader::new(&raw[..]);
        assert_eq!(read(&mut reader).await.unwrap(), Some(json!({})));
    }

    #[tokio::test]
    async fn a_body_that_is_cut_short_is_an_error() {
        let raw = &b"Content-Length: 10\r\n\r\n{}"[..];
        let mut reader = tokio::io::BufReader::new(raw);
        assert!(read(&mut reader).await.is_err());
    }

    // The length was readable, so the stream is still in step: the bad message is skipped.
    #[tokio::test]
    async fn a_body_that_is_not_json_is_skipped_and_the_next_message_is_read() {
        let mut bytes = b"Content-Length: 3\r\n\r\nabc".to_vec();
        bytes.extend(encode(&json!({"id": 7})));
        let mut reader = tokio::io::BufReader::new(&bytes[..]);
        assert_eq!(read(&mut reader).await.unwrap(), Some(json!({"id": 7})));
        assert_eq!(read(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_header_line_that_never_ends_is_refused_without_reading_it_all() {
        let raw = vec![b'x'; MAX_HEADER * 4];
        let mut reader = tokio::io::BufReader::new(&raw[..]);
        assert!(read(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn an_absurd_length_is_refused_before_it_is_allocated() {
        let raw = b"Content-Length: 99999999999\r\n\r\n";
        let mut reader = tokio::io::BufReader::new(&raw[..]);
        assert!(read(&mut reader).await.is_err());
    }
}
