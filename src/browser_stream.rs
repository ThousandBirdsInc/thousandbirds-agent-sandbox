//! DOM-diff browser streaming.
//!
//! An efficient alternative to VNC for *seeing and driving* the in-sandbox
//! Chromium. Instead of shipping a pixel framebuffer, we inject [rrweb] into the
//! page over the Chrome DevTools Protocol (CDP). rrweb serializes the DOM once
//! and then emits a compact stream of incremental mutation events (node inserts/
//! removes, attribute/text changes, scroll, input). Those events are fanned out
//! to a browser-based viewer that re-renders them with rrweb's `Replayer`, so
//! bandwidth tracks *DOM change*, not screen area — and it works headless (no
//! Xvfb/x11vnc).
//!
//! Interaction is a back-channel: the viewer captures pointer/keyboard/scroll
//! and we translate each event into CDP `Input.*` / `Page.navigate` calls
//! dispatched into the real browser.
//!
//! ```text
//! viewer ──http──> [serve()] ──CDP ws──> forwarded :9222 ──> guest Chromium
//!  Replayer <─ws──            addBinding / Runtime.bindingCalled ┘
//!  input ──ws──> Input.dispatch{Mouse,Key}Event / Page.navigate
//! ```
//!
//! Known lossy cases (deferred): `<canvas>`/WebGL and `<video>` pixels, and
//! cross-origin iframes. See `docs/browser-stream-design.md`.
//!
//! [rrweb]: https://github.com/rrweb-io/rrweb

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async, connect_async};

use crate::error::{Error, Result};

/// rrweb UMD bundle (record + Replayer), vendored and embedded so the binary is
/// self-contained. Injected into Chromium and also served to the viewer.
const RRWEB_JS: &str = include_str!("../assets/browser-stream/rrweb.min.js");
/// The viewer single-page app.
const VIEWER_HTML: &str = include_str!("../assets/browser-stream/viewer.html");

/// Name of the CDP binding the injected recorder calls to ship each event out.
const BINDING: &str = "__tbRrwebSend";

/// Serve the DOM-diff stream for the browser reachable via CDP at
/// `cdp_host:cdp_port`, accepting viewers on `listener`. Runs until the
/// listener or CDP connection fails (or the future is dropped).
pub async fn serve(cdp_host: String, cdp_port: u16, listener: TcpListener) -> Result<()> {
    // Outbound CDP commands funnel through one writer task; rrweb events fan out
    // to every viewer via a broadcast channel. These exist up front so viewers
    // can connect (and queue input) before CDP is even up.
    let (cdp_tx, cdp_rx) = mpsc::channel::<String>(256);
    let (events_tx, _) = broadcast::channel::<String>(8192);
    let id = Arc::new(AtomicU64::new(1));

    // Serve the viewer page immediately and concurrently bring up CDP — opening
    // the URL must never hang or 502 while Chromium's debugger is still racing
    // its `status=running`. Viewers just wait for events; the connect side
    // retries until the browser is reachable, then injects rrweb and pumps its
    // events to everyone.
    let accept = accept_viewers(listener, cdp_tx.clone(), events_tx.clone(), id.clone());
    let connect = connect_cdp(cdp_host, cdp_port, cdp_tx, cdp_rx, events_tx, id);
    tokio::select! {
        r = accept => r,
        r = connect => r,
    }
}

/// Accept and serve viewer connections forever (until the listener errors).
async fn accept_viewers(
    listener: TcpListener,
    cdp_tx: mpsc::Sender<String>,
    events_tx: broadcast::Sender<String>,
    id: Arc<AtomicU64>,
) -> Result<()> {
    loop {
        let (stream, _addr) = listener.accept().await.map_err(Error::io)?;
        let cdp_tx = cdp_tx.clone();
        let events_tx = events_tx.clone();
        let id = id.clone();
        tokio::spawn(async move {
            let _ = handle_client(stream, cdp_tx, events_tx, id).await;
        });
    }
}

/// Connect to Chromium's CDP (retrying until reachable), inject the recorder,
/// and pump rrweb events to all viewers. Returns when the CDP socket closes.
async fn connect_cdp(
    cdp_host: String,
    cdp_port: u16,
    cdp_tx: mpsc::Sender<String>,
    mut cdp_rx: mpsc::Receiver<String>,
    events_tx: broadcast::Sender<String>,
    id: Arc<AtomicU64>,
) -> Result<()> {
    // Chromium reports its own bind address in the debugger URL, so rewrite it to
    // the forwarded host:port we can actually reach.
    let ws_url = discover_page_ws(&cdp_host, cdp_port).await?;
    let (cdp_ws, _) = connect_async(&ws_url).await.map_err(ws_err)?;
    let (mut cdp_sink, mut cdp_stream) = cdp_ws.split();

    tokio::spawn(async move {
        while let Some(text) = cdp_rx.recv().await {
            if cdp_sink.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });

    // Enable domains, install the binding, and inject the recorder both for every
    // future document (survives navigation) and into the current one.
    send_cdp(&cdp_tx, &id, "Page.enable", json!({})).await;
    send_cdp(&cdp_tx, &id, "Runtime.enable", json!({})).await;
    send_cdp(
        &cdp_tx,
        &id,
        "Runtime.addBinding",
        json!({ "name": BINDING }),
    )
    .await;
    let source = injected_recorder();
    send_cdp(
        &cdp_tx,
        &id,
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": source }),
    )
    .await;
    send_cdp(
        &cdp_tx,
        &id,
        "Runtime.evaluate",
        json!({ "expression": source }),
    )
    .await;
    eprintln!("browser: CDP connected; streaming DOM to viewers.");

    // Read CDP frames; forward each rrweb event (delivered via the binding).
    while let Some(Ok(msg)) = cdp_stream.next().await {
        if let Message::Text(t) = msg {
            if let Some(payload) = binding_payload(t.as_str()) {
                let _ = events_tx.send(payload);
            }
        }
    }
    Ok(())
}

/// rrweb bundle + a bootstrap that starts recording and pipes every event to the
/// CDP binding. Idempotent so the new-document and current-document injections
/// don't double-start.
fn injected_recorder() -> String {
    format!(
        "{RRWEB_JS}\n;(function(){{\
           if (window.__tbRrwebStarted) return;\
           if (typeof rrweb === 'undefined' || !rrweb.record) return;\
           window.__tbRrwebStarted = true;\
           rrweb.record({{\
             emit: function(ev){{ try {{ window.{BINDING}(JSON.stringify(ev)); }} catch(e){{}} }},\
             recordCanvas: false,\
             collectFonts: true,\
             inlineStylesheet: true\
           }});\
         }})();"
    )
}

/// Ask the live recorder to emit a fresh full snapshot — sent when a viewer
/// connects so it gets a complete tree rather than only subsequent diffs.
const TAKE_SNAPSHOT: &str =
    "try{window.rrweb&&rrweb.record&&rrweb.record.takeFullSnapshot&&rrweb.record.takeFullSnapshot(true)}catch(e){}";

/// If `frame` is a `Runtime.bindingCalled` for our binding, return its payload
/// (one serialized rrweb event).
fn binding_payload(frame: &str) -> Option<String> {
    let v: Value = serde_json::from_str(frame).ok()?;
    if v.get("method")?.as_str()? != "Runtime.bindingCalled" {
        return None;
    }
    let params = v.get("params")?;
    if params.get("name")?.as_str()? != BINDING {
        return None;
    }
    Some(params.get("payload")?.as_str()?.to_string())
}

/// Poll the CDP HTTP endpoint until a `page` target is available, returning its
/// (rewritten) debugger WebSocket URL. Tolerates the gap between the browser
/// process spawning and its remote-debugging port accepting connections.
async fn discover_page_ws(host: &str, port: u16) -> Result<String> {
    let mut last_err = "no attempt".to_string();
    // ~40s total (160 × 250ms) — generous for a cold Chromium start.
    for attempt in 0..160 {
        match http_get(host, port, "/json").await {
            Ok(json) => match pick_page_ws(&json, host, port) {
                Ok(ws) => return Ok(ws),
                Err(e) => last_err = e.to_string(),
            },
            Err(e) => last_err = e.to_string(),
        }
        if attempt == 0 {
            eprintln!("browser: waiting for Chromium's CDP port to come up…");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(Error::Internal(format!(
        "Chromium CDP did not become ready at {host}:{port}: {last_err}"
    )))
}

/// Pick the first `page` target from a CDP `/json` listing and return its
/// debugger WebSocket URL, rewritten to the reachable `host:port`.
fn pick_page_ws(targets_json: &str, host: &str, port: u16) -> Result<String> {
    let targets: Value = serde_json::from_str(targets_json)
        .map_err(|e| Error::Internal(format!("CDP /json was not valid JSON: {e}")))?;
    let arr = targets
        .as_array()
        .ok_or_else(|| Error::Internal("CDP /json was not an array".to_string()))?;
    let ws = arr
        .iter()
        .find(|t| t.get("type").and_then(|x| x.as_str()) == Some("page"))
        .and_then(|t| t.get("webSocketDebuggerUrl"))
        .and_then(|x| x.as_str())
        .ok_or_else(|| {
            Error::Internal(
                "no CDP 'page' target with a webSocketDebuggerUrl (is the browser running?)"
                    .to_string(),
            )
        })?;
    Ok(rewrite_ws_authority(ws, host, port))
}

/// Replace the `host:port` authority of a `ws://host:port/path` URL with the
/// forwarded endpoint, preserving the path (the `/devtools/page/<id>` token).
fn rewrite_ws_authority(ws_url: &str, host: &str, port: u16) -> String {
    let rest = ws_url.strip_prefix("ws://").unwrap_or(ws_url);
    let path = match rest.find('/') {
        Some(i) => &rest[i..],
        None => "/",
    };
    format!("ws://{host}:{port}{path}")
}

/// Build a CDP command with a fresh id and enqueue it for the writer task.
async fn send_cdp(tx: &mpsc::Sender<String>, id: &AtomicU64, method: &str, params: Value) {
    let n = id.fetch_add(1, Ordering::Relaxed);
    let cmd = json!({ "id": n, "method": method, "params": params });
    let _ = tx.send(cmd.to_string()).await;
}

/// Route a freshly-accepted connection: WebSocket upgrade on `/ws`, otherwise
/// serve the viewer assets.
async fn handle_client(
    mut stream: TcpStream,
    cdp_tx: mpsc::Sender<String>,
    events_tx: broadcast::Sender<String>,
    id: Arc<AtomicU64>,
) -> Result<()> {
    let head = match peek_head(&stream).await? {
        Some(h) => h,
        None => return Ok(()),
    };
    let path = request_path(&head);
    if path.starts_with("/ws") && is_websocket_upgrade(&head) {
        // Only peeked so far — accept_async re-reads the handshake from the
        // socket.
        let ws = accept_async(stream).await.map_err(ws_err)?;
        return serve_viewer_ws(ws, cdp_tx, events_tx, id).await;
    }
    // Consume the request before responding: closing a socket that still has
    // unread bytes in its receive buffer makes the OS send RST instead of FIN,
    // which truncates the response on the client (a racy "empty page").
    drain_head(&mut stream).await?;
    if path == "/" || path == "/index.html" {
        write_http(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            VIEWER_HTML.as_bytes(),
        )
        .await
    } else if path == "/rrweb.js" {
        write_http(
            &mut stream,
            "200 OK",
            "application/javascript; charset=utf-8",
            RRWEB_JS.as_bytes(),
        )
        .await
    } else {
        write_http(&mut stream, "404 Not Found", "text/plain", b"not found\n").await
    }
}

/// Read and discard the request head (up to the blank line). Viewer requests are
/// bodyless GETs, so this consumes the whole request.
async fn drain_head(stream: &mut TcpStream) -> Result<()> {
    let mut acc = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = stream.read(&mut buf).await.map_err(Error::io)?;
        if n == 0 {
            return Ok(());
        }
        acc.extend_from_slice(&buf[..n]);
        if find_header_end(&acc).is_some() || acc.len() > 64 * 1024 {
            return Ok(());
        }
    }
}

/// Drive one connected viewer: stream rrweb events down, translate input up.
async fn serve_viewer_ws(
    ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    cdp_tx: mpsc::Sender<String>,
    events_tx: broadcast::Sender<String>,
    id: Arc<AtomicU64>,
) -> Result<()> {
    let (mut tx, mut rx) = ws.split();
    let mut events = events_tx.subscribe();
    // Give this viewer a complete tree immediately.
    send_cdp(
        &cdp_tx,
        &id,
        "Runtime.evaluate",
        json!({ "expression": TAKE_SNAPSHOT }),
    )
    .await;

    let forward = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => {
                    if tx.send(Message::Text(ev)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Text(t) => {
                if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                    for (method, params) in input_to_cdp(&v) {
                        send_cdp(&cdp_tx, &id, method, params).await;
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    forward.abort();
    Ok(())
}

/// Translate one viewer input message into the CDP command(s) that actuate it
/// in the real browser. Pure (no I/O) so it can be unit-tested.
fn input_to_cdp(v: &Value) -> Vec<(&'static str, Value)> {
    let m = v.get("modifiers").and_then(|x| x.as_i64()).unwrap_or(0);
    let num = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    match v.get("t").and_then(|x| x.as_str()) {
        Some("mouse") => {
            let typ = match v.get("a").and_then(|x| x.as_str()) {
                Some("down") => "mousePressed",
                Some("up") => "mouseReleased",
                Some("move") => "mouseMoved",
                _ => return vec![],
            };
            let button = v.get("button").and_then(|x| x.as_str()).unwrap_or("left");
            let mut p = json!({
                "type": typ, "x": num("x"), "y": num("y"), "modifiers": m,
            });
            if typ == "mouseMoved" {
                p["button"] = json!("none");
            } else {
                p["button"] = json!(button);
                p["buttons"] = json!(button_mask(button));
                p["clickCount"] = json!(1);
            }
            vec![("Input.dispatchMouseEvent", p)]
        }
        Some("wheel") => vec![(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel", "x": num("x"), "y": num("y"),
                "deltaX": num("dx"), "deltaY": num("dy"), "modifiers": m,
            }),
        )],
        // Absolute scroll sync: the viewer scrolls its local DOM copy instantly
        // and only tells the real browser where it ended up (for lazy-loaded
        // content and to keep click coordinates aligned).
        Some("scroll") => {
            let expr = format!("window.scrollTo({},{})", num("x"), num("y"));
            vec![("Runtime.evaluate", json!({ "expression": expr }))]
        }
        Some("key") => {
            let typ = match v.get("a").and_then(|x| x.as_str()) {
                Some("down") => "keyDown",
                Some("up") => "keyUp",
                _ => return vec![],
            };
            let kc = num("keyCode");
            let mut p = json!({
                "type": typ,
                "key": v.get("key").and_then(|x| x.as_str()).unwrap_or(""),
                "code": v.get("code").and_then(|x| x.as_str()).unwrap_or(""),
                "windowsVirtualKeyCode": kc,
                "nativeVirtualKeyCode": kc,
                "modifiers": m,
            });
            if typ == "keyDown" {
                if let Some(text) = v.get("text").and_then(|x| x.as_str()) {
                    if !text.is_empty() {
                        p["text"] = json!(text);
                    }
                }
            }
            vec![("Input.dispatchKeyEvent", p)]
        }
        Some("nav") => match v.get("url").and_then(|x| x.as_str()) {
            Some(url) => vec![("Page.navigate", json!({ "url": url }))],
            None => vec![],
        },
        _ => vec![],
    }
}

/// CDP `buttons` bitmask for a held button.
fn button_mask(button: &str) -> u8 {
    match button {
        "left" => 1,
        "right" => 2,
        "middle" => 4,
        _ => 0,
    }
}

// --- tiny HTTP helpers (plain ws://, localhost; mirrors src/preview.rs) ------

/// Peek (without consuming) until the request head is complete, returning it as
/// text. Peeking lets `accept_async` re-read the WebSocket handshake later.
async fn peek_head(stream: &TcpStream) -> Result<Option<String>> {
    let mut buf = vec![0u8; 16 * 1024];
    for _ in 0..200 {
        let n = stream.peek(&mut buf).await.map_err(Error::io)?;
        if n == 0 {
            return Ok(None);
        }
        if let Some(end) = find_header_end(&buf[..n]) {
            return Ok(Some(String::from_utf8_lossy(&buf[..end]).into_owned()));
        }
        if n >= buf.len() {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(None)
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

/// The path token of the request line (`GET <path> HTTP/1.1`).
fn request_path(head: &str) -> &str {
    head.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
}

fn is_websocket_upgrade(head: &str) -> bool {
    head.lines().skip(1).any(|line| match line.split_once(':') {
        Some((k, v)) => {
            k.trim().eq_ignore_ascii_case("upgrade") && v.trim().eq_ignore_ascii_case("websocket")
        }
        None => false,
    })
}

async fn write_http(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        len = body.len(),
    );
    stream
        .write_all(header.as_bytes())
        .await
        .map_err(Error::io)?;
    stream.write_all(body).await.map_err(Error::io)?;
    stream.flush().await.map_err(Error::io)?;
    // Half-close: flush our response and signal FIN before dropping, so the
    // client reliably receives the full body.
    let _ = stream.shutdown().await;
    Ok(())
}

/// Minimal blocking-free HTTP GET against the CDP HTTP endpoint, returning the
/// response body. Avoids pulling in a full HTTP client for one request.
async fn http_get(host: &str, port: u16, path: &str) -> Result<String> {
    let mut stream = TcpStream::connect((host, port)).await.map_err(Error::io)?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n\
         Accept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.map_err(Error::io)?;
    // Read by Content-Length rather than waiting for EOF: a half-open close
    // doesn't always propagate promptly through a port forward / in-guest relay,
    // so read_to_end would stall until the timeout on every request.
    let read = async {
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        let mut header_end: Option<usize> = None;
        let mut content_len: Option<usize> = None;
        loop {
            if header_end.is_none() {
                if let Some(end) = find_header_end(&raw) {
                    header_end = Some(end);
                    let head = String::from_utf8_lossy(&raw[..end]);
                    content_len = head.lines().find_map(|line| {
                        let (k, v) = line.split_once(':')?;
                        k.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    });
                }
            }
            if let (Some(end), Some(clen)) = (header_end, content_len) {
                if raw.len() >= end + clen {
                    return Ok::<String, std::io::Error>(
                        String::from_utf8_lossy(&raw[end..end + clen]).into_owned(),
                    );
                }
            }
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                // EOF before Content-Length was satisfied (or no length header).
                let end = header_end.unwrap_or(raw.len());
                return Ok(String::from_utf8_lossy(&raw[end..]).into_owned());
            }
            raw.extend_from_slice(&buf[..n]);
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .map_err(|_| Error::Internal(format!("timed out reading CDP {path}")))?
        .map_err(Error::io)
}

fn ws_err(e: tokio_tungstenite::tungstenite::Error) -> Error {
    Error::Internal(format!("CDP websocket error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_ws_authority_to_forwarded_endpoint() {
        assert_eq!(
            rewrite_ws_authority(
                "ws://localhost:9222/devtools/page/ABC123",
                "127.0.0.1",
                5311
            ),
            "ws://127.0.0.1:5311/devtools/page/ABC123"
        );
        // Already 127.0.0.1 but different (guest) port.
        assert_eq!(
            rewrite_ws_authority("ws://127.0.0.1:9222/devtools/page/X", "127.0.0.1", 40001),
            "ws://127.0.0.1:40001/devtools/page/X"
        );
    }

    #[test]
    fn picks_first_page_target() {
        let json = r#"[
            {"type":"background_page","webSocketDebuggerUrl":"ws://localhost:9222/devtools/page/bg"},
            {"type":"page","webSocketDebuggerUrl":"ws://localhost:9222/devtools/page/PAGE1"},
            {"type":"page","webSocketDebuggerUrl":"ws://localhost:9222/devtools/page/PAGE2"}
        ]"#;
        assert_eq!(
            pick_page_ws(json, "127.0.0.1", 7000).unwrap(),
            "ws://127.0.0.1:7000/devtools/page/PAGE1"
        );
    }

    #[test]
    fn errors_when_no_page_target() {
        let json = r#"[{"type":"worker","webSocketDebuggerUrl":"ws://x/y"}]"#;
        assert!(pick_page_ws(json, "127.0.0.1", 7000).is_err());
    }

    #[test]
    fn extracts_binding_payload() {
        let frame = r#"{"method":"Runtime.bindingCalled","params":{"name":"__tbRrwebSend","payload":"{\"type\":2}"}}"#;
        assert_eq!(binding_payload(frame).as_deref(), Some("{\"type\":2}"));
        // Wrong binding / wrong method are ignored.
        let other =
            r#"{"method":"Runtime.bindingCalled","params":{"name":"somethingElse","payload":"x"}}"#;
        assert!(binding_payload(other).is_none());
        assert!(binding_payload(r#"{"id":1,"result":{}}"#).is_none());
    }

    #[test]
    fn mouse_down_maps_to_pressed_with_button() {
        let v = json!({"t":"mouse","a":"down","x":12.0,"y":34.0,"button":"left","modifiers":8});
        let out = input_to_cdp(&v);
        assert_eq!(out.len(), 1);
        let (method, p) = &out[0];
        assert_eq!(*method, "Input.dispatchMouseEvent");
        assert_eq!(p["type"], "mousePressed");
        assert_eq!(p["x"], 12.0);
        assert_eq!(p["button"], "left");
        assert_eq!(p["buttons"], 1);
        assert_eq!(p["clickCount"], 1);
        assert_eq!(p["modifiers"], 8);
    }

    #[test]
    fn mouse_move_has_no_button_press() {
        let v = json!({"t":"mouse","a":"move","x":1.0,"y":2.0});
        let (_, p) = &input_to_cdp(&v)[0];
        assert_eq!(p["type"], "mouseMoved");
        assert_eq!(p["button"], "none");
        assert!(p.get("clickCount").is_none());
    }

    #[test]
    fn key_down_carries_text_and_vk() {
        let v = json!({"t":"key","a":"down","key":"a","code":"KeyA","keyCode":65,"text":"a"});
        let (method, p) = &input_to_cdp(&v)[0];
        assert_eq!(*method, "Input.dispatchKeyEvent");
        assert_eq!(p["type"], "keyDown");
        assert_eq!(p["text"], "a");
        assert_eq!(p["windowsVirtualKeyCode"], 65.0);
    }

    #[test]
    fn key_up_omits_text() {
        let v = json!({"t":"key","a":"up","key":"a","code":"KeyA","keyCode":65,"text":"a"});
        let (_, p) = &input_to_cdp(&v)[0];
        assert_eq!(p["type"], "keyUp");
        assert!(p.get("text").is_none());
    }

    #[test]
    fn scroll_sync_maps_to_scroll_to() {
        let v = json!({"t":"scroll","x":0.0,"y":1200.0});
        let out = input_to_cdp(&v);
        assert_eq!(out.len(), 1);
        let (method, p) = &out[0];
        assert_eq!(*method, "Runtime.evaluate");
        assert_eq!(p["expression"], "window.scrollTo(0,1200)");
    }

    #[test]
    fn wheel_and_nav_map_through() {
        let wheel = json!({"t":"wheel","x":5.0,"y":6.0,"dx":0.0,"dy":120.0});
        let (m, p) = &input_to_cdp(&wheel)[0];
        assert_eq!(*m, "Input.dispatchMouseEvent");
        assert_eq!(p["type"], "mouseWheel");
        assert_eq!(p["deltaY"], 120.0);

        let nav = json!({"t":"nav","url":"https://example.com"});
        let (m, p) = &input_to_cdp(&nav)[0];
        assert_eq!(*m, "Page.navigate");
        assert_eq!(p["url"], "https://example.com");

        assert!(input_to_cdp(&json!({"t":"nav"})).is_empty());
        assert!(input_to_cdp(&json!({"t":"unknown"})).is_empty());
    }

    #[test]
    fn parses_request_line_and_upgrade() {
        let head =
            "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
        assert_eq!(request_path(head), "/ws");
        assert!(is_websocket_upgrade(head));

        let plain = "GET /rrweb.js HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(request_path(plain), "/rrweb.js");
        assert!(!is_websocket_upgrade(plain));
    }
}
