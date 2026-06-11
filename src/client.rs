//! Minimal async HTTP client for the localhost API server (Daytona gap plan,
//! Phase 2). Used by the CLI's API mode (`TB_SANDBOX_API_URL`) and by the HTTP
//! conformance tests so the same code path that ships is the one under test.
//!
//! Like [`crate::api`], this is hand-rolled on Tokio with no HTTP dependency.
//! It understands both `Content-Length` and `Transfer-Encoding: chunked`
//! responses; the streaming `exec` endpoint uses the chunked path and surfaces
//! each newline-delimited JSON event through a callback.

use crate::api::Json;
use crate::error::{Error, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// A client bound to one API server endpoint.
#[derive(Debug, Clone)]
pub struct ApiClient {
    host: String,
    port: u16,
    token: Option<String>,
}

/// One decoded event from a streaming `exec` response.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecEvent {
    Started { process_id: String },
    Stdout(String),
    Stderr(String),
    Exit { code: Option<i32>, success: bool },
    Error(String),
}

impl ApiClient {
    /// Parse a base URL of the form `http://host:port` (the scheme is optional
    /// and ignored; TLS is not used on the localhost surface).
    pub fn new(base_url: &str, token: Option<String>) -> Result<Self> {
        let trimmed = base_url
            .trim()
            .trim_end_matches('/')
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let (host, port) = match trimmed.rsplit_once(':') {
            Some((host, port)) => {
                let port = port.parse().map_err(|_| {
                    Error::InvalidArgument(format!("invalid API port in URL: {base_url}"))
                })?;
                (host.to_string(), port)
            }
            None => (trimmed.to_string(), 80),
        };
        if host.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "invalid API URL: {base_url}"
            )));
        }
        Ok(Self { host, port, token })
    }

    async fn connect(&self) -> Result<TcpStream> {
        TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|err| {
                Error::Internal(format!(
                    "could not connect to API server at {}:{}: {err}",
                    self.host, self.port
                ))
            })
    }

    fn request_head(&self, method: &str, path: &str, content_length: Option<usize>) -> String {
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n",
            self.host, self.port
        );
        if let Some(token) = &self.token {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if let Some(len) = content_length {
            head.push_str("Content-Type: application/json\r\n");
            head.push_str(&format!("Content-Length: {len}\r\n"));
        }
        head.push_str("\r\n");
        head
    }

    /// Send a request with an optional JSON body and return the parsed response
    /// body. Non-2xx responses are mapped back into [`Error`] using the
    /// `error.category` field.
    pub(crate) async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<Json> {
        let mut stream = self.connect().await?;
        let head = self.request_head(method, path, body.map(|b| b.len()));
        stream.write_all(head.as_bytes()).await.map_err(Error::io)?;
        if let Some(body) = body {
            stream.write_all(body.as_bytes()).await.map_err(Error::io)?;
        }
        stream.flush().await.map_err(Error::io)?;

        let mut reader = BufReader::new(stream);
        let (status, _headers, is_chunked, content_length) =
            read_response_head(&mut reader).await?;
        let body_bytes = read_body(&mut reader, is_chunked, content_length).await?;
        let text = String::from_utf8_lossy(&body_bytes).into_owned();
        let json = Json::parse(&text).unwrap_or(Json::Null);
        if (200..300).contains(&status) {
            Ok(json)
        } else {
            Err(error_from_response(status, &json))
        }
    }

    pub(crate) async fn get(&self, path: &str) -> Result<Json> {
        self.request("GET", path, None).await
    }

    pub(crate) async fn post(&self, path: &str, body: &str) -> Result<Json> {
        self.request("POST", path, Some(body)).await
    }

    pub(crate) async fn delete(&self, path: &str) -> Result<Json> {
        self.request("DELETE", path, None).await
    }

    /// Stream a process execution, invoking `on_event` for each decoded event.
    /// Returns the final exit status (code, success) once the stream ends.
    pub async fn exec_stream<F>(
        &self,
        sandbox_id: &str,
        body: &str,
        mut on_event: F,
    ) -> Result<(Option<i32>, bool)>
    where
        F: FnMut(&ExecEvent),
    {
        let path = format!("/v1/sandboxes/{sandbox_id}/exec");
        let mut stream = self.connect().await?;
        let head = self.request_head("POST", &path, Some(body.len()));
        stream.write_all(head.as_bytes()).await.map_err(Error::io)?;
        stream.write_all(body.as_bytes()).await.map_err(Error::io)?;
        stream.flush().await.map_err(Error::io)?;

        let mut reader = BufReader::new(stream);
        let (status, _headers, is_chunked, content_length) =
            read_response_head(&mut reader).await?;
        if !(200..300).contains(&status) {
            let body_bytes = read_body(&mut reader, is_chunked, content_length).await?;
            let text = String::from_utf8_lossy(&body_bytes).into_owned();
            let json = Json::parse(&text).unwrap_or(Json::Null);
            return Err(error_from_response(status, &json));
        }

        let mut exit = (None, false);
        let mut dispatch_line = |line: &str, exit: &mut (Option<i32>, bool)| {
            if line.trim().is_empty() {
                return;
            }
            if let Some(parsed) = Json::parse(line).as_ref().and_then(decode_exec_event) {
                if let ExecEvent::Exit { code, success } = &parsed {
                    *exit = (*code, *success);
                }
                on_event(&parsed);
            }
        };

        if is_chunked {
            // Decode NDJSON events as chunks arrive so callers see output (and a
            // process id) live, rather than only after the process exits.
            let mut buffer = String::new();
            while let Some(chunk) = read_one_chunk(&mut reader).await? {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(newline) = buffer.find('\n') {
                    let line: String = buffer.drain(..=newline).collect();
                    dispatch_line(&line, &mut exit);
                }
            }
            if !buffer.is_empty() {
                dispatch_line(&buffer, &mut exit);
            }
        } else {
            let body_bytes = read_body(&mut reader, is_chunked, content_length).await?;
            let text = String::from_utf8_lossy(&body_bytes).into_owned();
            for line in text.lines() {
                dispatch_line(line, &mut exit);
            }
        }
        Ok(exit)
    }
}

/// Read a single HTTP chunk from a chunked response body. Returns `None` at the
/// terminating zero-length chunk or on EOF.
async fn read_one_chunk<R>(reader: &mut BufReader<R>) -> Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut size_line = String::new();
    if reader.read_line(&mut size_line).await.map_err(Error::io)? == 0 {
        return Ok(None);
    }
    let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
    if size == 0 {
        let mut trailer = String::new();
        let _ = reader.read_line(&mut trailer).await;
        return Ok(None);
    }
    let mut chunk = vec![0u8; size];
    reader.read_exact(&mut chunk).await.map_err(Error::io)?;
    let mut crlf = [0u8; 2];
    let _ = reader.read_exact(&mut crlf).await;
    Ok(Some(chunk))
}

fn decode_exec_event(event: &Json) -> Option<ExecEvent> {
    match event.get("type").and_then(Json::as_str)? {
        "started" => Some(ExecEvent::Started {
            process_id: event.get("process_id").and_then(Json::as_str)?.to_string(),
        }),
        "stdout" => Some(ExecEvent::Stdout(
            event
                .get("data")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
        )),
        "stderr" => Some(ExecEvent::Stderr(
            event
                .get("data")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
        )),
        "exit" => Some(ExecEvent::Exit {
            code: event.get("code").and_then(Json::as_u64).map(|c| c as i32),
            success: event
                .get("success")
                .and_then(Json::as_bool)
                .unwrap_or(false),
        }),
        "error" => Some(ExecEvent::Error(
            event
                .get("message")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
        )),
        _ => None,
    }
}

fn error_from_response(status: u16, json: &Json) -> Error {
    let error = json.get("error");
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Json::as_str)
        .unwrap_or("request failed")
        .to_string();
    let category = error
        .and_then(|e| e.get("category"))
        .and_then(Json::as_str)
        .unwrap_or("");
    match category {
        "not_found" => Error::NotFound(message),
        "invalid_argument" => Error::InvalidArgument(message),
        "resource_exhausted" => Error::ResourceExhausted(message),
        _ => Error::Internal(format!("API error ({status}): {message}")),
    }
}

async fn read_response_head<R>(
    reader: &mut BufReader<R>,
) -> Result<(u16, Vec<(String, String)>, bool, Option<usize>)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .await
        .map_err(Error::io)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| Error::Internal(format!("malformed status line: {status_line:?}")))?;

    let mut headers = Vec::new();
    let mut is_chunked = false;
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.map_err(Error::io)? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if key == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                is_chunked = true;
            }
            if key == "content-length" {
                content_length = value.parse().ok();
            }
            headers.push((key, value));
        }
    }
    Ok((status, headers, is_chunked, content_length))
}

async fn read_body<R>(
    reader: &mut BufReader<R>,
    is_chunked: bool,
    content_length: Option<usize>,
) -> Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    if is_chunked {
        let mut body = Vec::new();
        loop {
            let mut size_line = String::new();
            if reader.read_line(&mut size_line).await.map_err(Error::io)? == 0 {
                break;
            }
            let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
            if size == 0 {
                // consume the trailing CRLF after the final chunk
                let mut trailer = String::new();
                let _ = reader.read_line(&mut trailer).await;
                break;
            }
            let mut chunk = vec![0u8; size];
            reader.read_exact(&mut chunk).await.map_err(Error::io)?;
            body.extend_from_slice(&chunk);
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf).await;
        }
        Ok(body)
    } else if let Some(len) = content_length {
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).await.map_err(Error::io)?;
        Ok(body)
    } else {
        // No length and not chunked: read to EOF (Connection: close).
        let mut body = Vec::new();
        reader.read_to_end(&mut body).await.map_err(Error::io)?;
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_base_urls() {
        let client = ApiClient::new("http://127.0.0.1:8080", None).unwrap();
        assert_eq!(client.host, "127.0.0.1");
        assert_eq!(client.port, 8080);
        let client = ApiClient::new("localhost:9000/", Some("t".into())).unwrap();
        assert_eq!(client.host, "localhost");
        assert_eq!(client.port, 9000);
        assert!(ApiClient::new("http://:8080", None).is_err());
    }
}
