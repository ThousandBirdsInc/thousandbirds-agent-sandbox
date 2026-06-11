//! Preview reverse proxy (Daytona gap plan, Phase 4).
//!
//! A localhost reverse proxy that exposes a guest's HTTP/WebSocket servers to a
//! browser or tool without weakening the VM model. Requests are routed by their
//! `Host` header of the form `{guest_port}-{sandbox_id}.localhost[:proxy_port]`:
//! the proxy resolves a host-side port forward for that sandbox/port through the
//! API server and splices the connection to it. The raw bidirectional splice
//! means HTTP/1.1, streaming responses, and WebSocket upgrades all work without
//! the proxy needing to understand the application protocol.
//!
//! Access is gated by a preview token — distinct from the control-plane API
//! token — presented as `Authorization: Bearer <token>` or a `?token=<token>`
//! query parameter so plain browser links work. The credential is stripped from
//! the request head before it is forwarded to the (untrusted) guest upstream.
//! `require_token = None` disables the check.

use crate::client::ApiClient;
use crate::error::{Error, Result};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Maximum bytes we will buffer while reading the request head before the
/// blank line. Guards against unbounded memory use on a malformed client.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// A running preview proxy. Routes are resolved live through the API client, so
/// forwards appear and disappear with the sandboxes the API server tracks.
pub struct PreviewProxy {
    inner: Arc<ProxyInner>,
}

struct ProxyInner {
    client: ApiClient,
    require_token: Option<String>,
}

impl PreviewProxy {
    pub fn new(client: ApiClient, require_token: Option<String>) -> Self {
        Self {
            inner: Arc::new(ProxyInner {
                client,
                require_token,
            }),
        }
    }

    pub async fn serve(self, listener: TcpListener) -> Result<()> {
        loop {
            let (stream, _peer) = listener.accept().await.map_err(Error::io)?;
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move {
                if let Err(err) = handle_connection(stream, inner).await {
                    if err.kind() != std::io::ErrorKind::BrokenPipe {
                        eprintln!("tb-sandbox preview: connection error: {err}");
                    }
                }
            });
        }
    }
}

async fn handle_connection(mut client: TcpStream, inner: Arc<ProxyInner>) -> std::io::Result<()> {
    let head = match read_head(&mut client).await? {
        Some(head) => head,
        None => return Ok(()),
    };

    let request_line = head.text.lines().next().unwrap_or_default();
    let target = request_line.split_whitespace().nth(1).unwrap_or("");

    // Authentication: bearer header or ?token= query parameter.
    if let Some(expected) = &inner.require_token {
        let presented = bearer_token(&head.text).or_else(|| query_token(target));
        if presented.as_deref() != Some(expected.as_str()) {
            return write_status(
                &mut client,
                401,
                "Unauthorized",
                "missing or invalid preview token",
            )
            .await;
        }
    }

    let Some(host) = header_value(&head.text, "host") else {
        return write_status(&mut client, 400, "Bad Request", "missing Host header").await;
    };
    let Some((guest_port, sandbox_id)) = parse_preview_host(&host) else {
        return write_status(
            &mut client,
            400,
            "Bad Request",
            "Host must be {port}-{sandbox_id}.localhost",
        )
        .await;
    };

    // Resolve a host-side forward for this sandbox/port through the API.
    let upstream_addr = match resolve_forward(&inner.client, &sandbox_id, guest_port).await {
        Ok(addr) => addr,
        Err(err) => {
            let (code, reason) = match err {
                Error::NotFound(_) => (404, "Not Found"),
                Error::InvalidArgument(_) => (400, "Bad Request"),
                _ => (502, "Bad Gateway"),
            };
            return write_status(&mut client, code, reason, &err.to_string()).await;
        }
    };

    let mut upstream = match TcpStream::connect(&upstream_addr).await {
        Ok(stream) => stream,
        Err(err) => {
            return write_status(
                &mut client,
                502,
                "Bad Gateway",
                &format!("could not reach forward at {upstream_addr}: {err}"),
            )
            .await;
        }
    };

    // Replay the bytes we already consumed (head + any body prefix), then splice
    // both directions for the remainder of the connection. This transparently
    // carries streaming responses and WebSocket upgrades.
    //
    // The upstream is a server running INSIDE the untrusted sandbox, so the
    // preview credential must never reach it: strip the `?token=` query
    // parameter and a matching `Authorization: Bearer` header from the head
    // before forwarding. Otherwise guest-controlled code could read the token
    // off the request line and replay it against the host control plane.
    let forwarded = strip_preview_credentials(&head, inner.require_token.as_deref());
    upstream.write_all(&forwarded).await?;
    upstream.flush().await?;
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(err) => Err(err),
    }
}

/// Resolve the host endpoint (`host:port`) of a forward for `guest_port`,
/// creating it through the API if needed.
async fn resolve_forward(client: &ApiClient, sandbox_id: &str, guest_port: u16) -> Result<String> {
    let body = format!("{{\"guest_port\":{guest_port}}}");
    let response = client
        .post(&format!("/v1/sandboxes/{sandbox_id}/ports"), &body)
        .await?;
    let host = response
        .get("host")
        .and_then(crate::api::Json::as_str)
        .unwrap_or("127.0.0.1");
    let port = response
        .get("port")
        .and_then(crate::api::Json::as_u64)
        .ok_or_else(|| Error::Internal("forward response missing port".to_string()))?;
    Ok(format!("{host}:{port}"))
}

struct Head {
    /// All bytes read up to and including the blank line, plus any pipelined
    /// body bytes that arrived in the same read.
    raw: Vec<u8>,
    /// UTF-8 (lossy) view of the header section for parsing.
    text: String,
}

async fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<Head>> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_header_end(&raw) {
            let text = String::from_utf8_lossy(&raw[..pos]).into_owned();
            return Ok(Some(Head { raw, text }));
        }
        if raw.len() > MAX_HEAD_BYTES {
            return Ok(None);
        }
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

/// Rebuild the request head with the preview credential removed, then re-append
/// any pipelined body bytes. When `token` is `None` (preview auth disabled)
/// there is no secret to scrub, so the original bytes are forwarded verbatim.
///
/// Scrubbing is precise: only the `token` query parameter and an
/// `Authorization: Bearer <token>` header carrying the preview token are
/// dropped, so an application's own query params and credentials still reach it.
fn strip_preview_credentials(head: &Head, token: Option<&str>) -> Vec<u8> {
    let Some(token) = token else {
        return head.raw.clone();
    };

    // Split the header section from any pipelined body that arrived with it.
    let header_end = find_header_end(&head.raw).unwrap_or(head.raw.len());
    let body = &head.raw[header_end..];

    // `head.text` is the header block (CRLF-separated, ending in the blank
    // line). Header bytes are ASCII in practice, so rebuilding from the lossy
    // text view is faithful here while the raw body bytes are preserved exactly.
    let mut out = String::with_capacity(head.text.len());
    let mut lines = head.text.split("\r\n");
    if let Some(request_line) = lines.next() {
        out.push_str(&strip_request_line_token(request_line));
        out.push_str("\r\n");
    }
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if is_preview_bearer(line, token) {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");

    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// Remove the `token` query parameter from a request line's target, preserving
/// the method, version, and all other query parameters.
fn strip_request_line_token(request_line: &str) -> String {
    let mut parts = request_line.splitn(3, ' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return request_line.to_string();
    };
    let sanitized = strip_target_token(target);
    match parts.next() {
        Some(version) => format!("{method} {sanitized} {version}"),
        None => format!("{method} {sanitized}"),
    }
}

fn strip_target_token(target: &str) -> String {
    let Some((path, query)) = target.split_once('?') else {
        return target.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let key = pair.split_once('=').map(|(k, _)| k).unwrap_or(pair);
            key != "token"
        })
        .collect();
    if kept.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", kept.join("&"))
    }
}

/// True if `line` is an `Authorization` header carrying the preview bearer
/// token (so it should be dropped before forwarding to the untrusted upstream).
fn is_preview_bearer(line: &str, token: &str) -> bool {
    let Some((name, value)) = line.split_once(':') else {
        return false;
    };
    if !name.trim().eq_ignore_ascii_case("authorization") {
        return false;
    }
    value
        .trim()
        .strip_prefix("Bearer ")
        .is_some_and(|bearer| bearer.trim() == token)
}

fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case(name) {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

fn bearer_token(head: &str) -> Option<String> {
    header_value(head, "authorization")?
        .strip_prefix("Bearer ")
        .map(|token| token.trim().to_string())
}

fn query_token(target: &str) -> Option<String> {
    let (_, query) = target.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "token").then(|| value.to_string())
    })
}

/// Parse a preview Host header into `(guest_port, sandbox_id)`.
///
/// Accepts `{port}-{sandbox_id}.localhost` with an optional `:proxy_port`
/// suffix (the port the client connected to). The sandbox id may itself contain
/// hyphens, so only the leading numeric run before the first `-` is the port.
fn parse_preview_host(host: &str) -> Option<(u16, String)> {
    // Strip the connection port suffix (`...localhost:18200`).
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    let labels = host.strip_suffix(".localhost").unwrap_or(host);
    let (port_str, sandbox_id) = labels.split_once('-')?;
    if port_str.is_empty() || sandbox_id.is_empty() {
        return None;
    }
    let port: u16 = port_str.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some((port, sandbox_id.to_string()))
}

async fn write_status(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    message: &str,
) -> std::io::Result<()> {
    let body = format!("{code} {reason}: {message}\n");
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        len = body.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_preview_hosts() {
        assert_eq!(
            parse_preview_host("8080-mybox.localhost"),
            Some((8080, "mybox".to_string()))
        );
        // sandbox id with hyphens, plus a connection port suffix
        assert_eq!(
            parse_preview_host("3000-contract-http.localhost:18200"),
            Some((3000, "contract-http".to_string()))
        );
        // without the .localhost suffix
        assert_eq!(parse_preview_host("9-x"), Some((9, "x".to_string())));
        assert_eq!(parse_preview_host("nope.localhost"), None);
        assert_eq!(parse_preview_host("0-x.localhost"), None);
        assert_eq!(parse_preview_host("-x.localhost"), None);
    }

    #[test]
    fn extracts_tokens() {
        assert_eq!(
            bearer_token("GET / HTTP/1.1\r\nAuthorization: Bearer abc\r\n"),
            Some("abc".to_string())
        );
        assert_eq!(
            query_token("/path?a=1&token=xyz&b=2"),
            Some("xyz".to_string())
        );
        assert_eq!(query_token("/path"), None);
    }

    fn head_from(raw: &[u8]) -> Head {
        let pos = find_header_end(raw).unwrap_or(raw.len());
        Head {
            raw: raw.to_vec(),
            text: String::from_utf8_lossy(&raw[..pos]).into_owned(),
        }
    }

    #[test]
    fn strips_token_from_query_and_authorization() {
        // Both the ?token= query param and a matching Bearer header must be
        // gone from the bytes forwarded to the untrusted upstream.
        let raw = b"GET /app?token=SECRET&x=1 HTTP/1.1\r\n\
                    Host: 8080-box.localhost\r\n\
                    Authorization: Bearer SECRET\r\n\
                    \r\nBODYBYTES";
        let head = head_from(raw);
        let out = strip_preview_credentials(&head, Some("SECRET"));
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("SECRET"), "token leaked to upstream: {text}");
        assert!(text.starts_with("GET /app?x=1 HTTP/1.1\r\n"));
        assert!(!text.to_ascii_lowercase().contains("authorization"));
        // Unrelated query params and the body are preserved untouched.
        assert!(text.contains("x=1"));
        assert!(text.ends_with("\r\nBODYBYTES"));
    }

    #[test]
    fn strips_lone_token_query_param() {
        let raw = b"GET /?token=SECRET HTTP/1.1\r\nHost: 8080-box.localhost\r\n\r\n";
        let head = head_from(raw);
        let out = strip_preview_credentials(&head, Some("SECRET"));
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("GET / HTTP/1.1\r\n"), "got: {text}");
        assert!(!text.contains("SECRET"));
    }

    #[test]
    fn preserves_application_authorization_and_params() {
        // An app's own (non-preview) credentials and params must pass through.
        let raw = b"GET /api?user=alice HTTP/1.1\r\n\
                    Host: 8080-box.localhost\r\n\
                    Authorization: Bearer APP_TOKEN\r\n\r\n";
        let head = head_from(raw);
        let out = strip_preview_credentials(&head, Some("SECRET"));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Authorization: Bearer APP_TOKEN"));
        assert!(text.contains("user=alice"));
    }

    #[test]
    fn forwards_verbatim_when_auth_disabled() {
        let raw = b"GET /?token=whatever HTTP/1.1\r\nHost: 8080-box.localhost\r\n\r\n";
        let head = head_from(raw);
        let out = strip_preview_credentials(&head, None);
        assert_eq!(out, raw.to_vec());
    }

    #[test]
    fn find_header_end_locates_blank_line() {
        assert_eq!(
            find_header_end(b"GET / HTTP/1.1\r\nHost: x\r\n\r\nBODY"),
            Some(27)
        );
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n"), None);
    }

    // -- end-to-end routing -------------------------------------------------

    use crate::api::{ApiServer, Auth};
    use crate::config::{AgentImageRef, SandboxConfig, WorkspaceMount};
    use crate::sdk::{LocalProcessSandboxRuntime, SandboxRuntimeSdk};

    /// A one-shot backend that returns a fixed body on the next connection.
    async fn spawn_backend(body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        port
    }

    async fn http_get(addr: &str, target: &str, host: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let _ = stream.shutdown().await;
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn preview_proxy_routes_host_to_forward() {
        let root = std::env::temp_dir().join(format!("tb-preview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // API server over the in-process runtime; its forward_port returns the
        // guest port verbatim on 127.0.0.1, which we point at a real backend.
        let runtime = LocalProcessSandboxRuntime::new(&root);
        let mut config = SandboxConfig::new(AgentImageRef::new(root.join("image")));
        config.name = Some("previewbox".to_string());
        config
            .workspace_mounts
            .push(WorkspaceMount::workspace(root.join("ws")));
        runtime.create_session(config).await.unwrap();

        let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_addr = api_listener.local_addr().unwrap();
        let server = ApiServer::new(Arc::new(runtime), Auth::Bearer("preview-token".to_string()));
        tokio::spawn(async move {
            let _ = server.serve(api_listener).await;
        });

        let backend_port = spawn_backend("hello-from-guest").await;

        let client = ApiClient::new(
            &format!("http://{api_addr}"),
            Some("preview-token".to_string()),
        )
        .unwrap();
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let proxy = PreviewProxy::new(client, Some("preview-token".to_string()));
        tokio::spawn(async move {
            let _ = proxy.serve(proxy_listener).await;
        });

        // Authorized request routed by Host -> reaches the backend.
        let host = format!("{backend_port}-previewbox.localhost:{}", proxy_addr.port());
        let response = http_get(&proxy_addr.to_string(), "/?token=preview-token", &host).await;
        assert!(
            response.contains("hello-from-guest"),
            "expected backend body, got: {response}"
        );

        // Missing token -> 401, no routing.
        let denied = http_get(&proxy_addr.to_string(), "/", &host).await;
        assert!(denied.contains("401"), "expected 401, got: {denied}");

        // Unknown sandbox -> resolved as not found.
        let bad_host = format!("{backend_port}-nosuchbox.localhost:{}", proxy_addr.port());
        let missing = http_get(&proxy_addr.to_string(), "/?token=preview-token", &bad_host).await;
        assert!(missing.contains("404"), "expected 404, got: {missing}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
