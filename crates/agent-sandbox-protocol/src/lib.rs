//! Wire protocol shared by the tb-sandbox host SDK (`DaemonClient`) and the
//! in-guest control daemon (`agent-sandbox-daemon`).
//!
//! The transport is a single bidirectional byte stream — an `AF_VSOCK`
//! connection in production (bridged to a host `AF_UNIX` socket by libkrun's
//! `krun_add_vsock_port2(.., listen=true)`), or a plain `AF_UNIX` socket in
//! tests. Multiple logical operations are multiplexed over the one connection,
//! keyed by a per-operation `id`.
//!
//! Framing: each message is a `u32` big-endian length prefix followed by a
//! JSON object payload. Binary data (process stdout/stderr, PTY bytes, file
//! contents) travels base64-encoded in a string field so the payload stays
//! valid JSON and dependency-light (no separate binary codec).
//!
//! This crate is intentionally free of any libkrun / VMM dependency so it
//! cross-compiles cleanly into the small static guest binary.

use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Vsock port the guest daemon listens on and the host connects to (bridged
/// through the per-sandbox `control.vsock` unix socket).
pub const DAEMON_VSOCK_PORT: u32 = 1024;

/// Maximum accepted frame payload size (guards against a corrupt length
/// prefix). 64 MiB comfortably covers a single file chunk or output burst.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

/// A single protocol message: always a JSON object carrying a string tag `t`
/// and a numeric correlation `id`. Payload fields are accessed through typed
/// helpers rather than a large hand-maintained enum, which keeps the message
/// set easy to extend without a serde-derive dependency in the guest binary.
#[derive(Debug, Clone, PartialEq)]
pub struct Message(pub Value);

impl Message {
    /// Build a new message with tag `t` and correlation `id`.
    pub fn new(t: &str, id: u64) -> Self {
        let mut map = Map::new();
        map.insert("t".to_string(), Value::String(t.to_string()));
        map.insert("id".to_string(), Value::from(id));
        Message(Value::Object(map))
    }

    fn map_mut(&mut self) -> &mut Map<String, Value> {
        // Construction always uses an object; the only way to get a Message is
        // `new` or `decode` (which validates object-ness), so this is total.
        self.0.as_object_mut().expect("message is a json object")
    }

    fn map(&self) -> &Map<String, Value> {
        self.0.as_object().expect("message is a json object")
    }

    /// Set a string field, returning self for chaining.
    pub fn with_str(mut self, key: &str, value: &str) -> Self {
        self.map_mut()
            .insert(key.to_string(), Value::String(value.to_string()));
        self
    }

    /// Set an optional string field; omitted entirely when `None`.
    pub fn with_opt_str(self, key: &str, value: Option<&str>) -> Self {
        match value {
            Some(v) => self.with_str(key, v),
            None => self,
        }
    }

    /// Set an unsigned-integer field.
    pub fn with_u64(mut self, key: &str, value: u64) -> Self {
        self.map_mut().insert(key.to_string(), Value::from(value));
        self
    }

    /// Set a signed-integer field.
    pub fn with_i64(mut self, key: &str, value: i64) -> Self {
        self.map_mut().insert(key.to_string(), Value::from(value));
        self
    }

    /// Set a boolean field.
    pub fn with_bool(mut self, key: &str, value: bool) -> Self {
        self.map_mut().insert(key.to_string(), Value::Bool(value));
        self
    }

    /// Set a binary field, stored base64-encoded under `key`.
    pub fn with_bytes(mut self, key: &str, value: &[u8]) -> Self {
        self.map_mut()
            .insert(key.to_string(), Value::String(base64_encode(value)));
        self
    }

    /// Set an arbitrary JSON value field.
    pub fn with_value(mut self, key: &str, value: Value) -> Self {
        self.map_mut().insert(key.to_string(), value);
        self
    }

    /// The message tag.
    pub fn t(&self) -> &str {
        self.map().get("t").and_then(Value::as_str).unwrap_or("")
    }

    /// The correlation id.
    pub fn id(&self) -> u64 {
        self.map().get("id").and_then(Value::as_u64).unwrap_or(0)
    }

    /// A string field, if present.
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.map().get(key).and_then(Value::as_str)
    }

    /// An unsigned-integer field, if present.
    pub fn u64_field(&self, key: &str) -> Option<u64> {
        self.map().get(key).and_then(Value::as_u64)
    }

    /// A signed-integer field, if present.
    pub fn i64_field(&self, key: &str) -> Option<i64> {
        self.map().get(key).and_then(Value::as_i64)
    }

    /// A boolean field (absent → `false`).
    pub fn bool_field(&self, key: &str) -> bool {
        self.map().get(key).and_then(Value::as_bool).unwrap_or(false)
    }

    /// A binary field, base64-decoded. Absent or malformed → `None`.
    pub fn bytes_field(&self, key: &str) -> Option<Vec<u8>> {
        self.str_field(key).and_then(|s| base64_decode(s).ok())
    }

    /// A raw JSON value field, if present.
    pub fn value_field(&self, key: &str) -> Option<&Value> {
        self.map().get(key)
    }
}

/// Write one framed message to an async writer.
pub async fn write_message<W>(writer: &mut W, message: &Message) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let payload = serde_json::to_vec(&message.0)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    if payload.len() > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "outgoing frame exceeds MAX_FRAME_LEN",
        ));
    }
    let len = (payload.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one framed message from an async reader. Returns `Ok(None)` on a clean
/// EOF at a frame boundary.
pub async fn read_message<R>(reader: &mut R) -> std::io::Result<Option<Message>>
where
    R: AsyncReadExt + Unpin,
{
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "incoming frame exceeds MAX_FRAME_LEN",
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    let value: Value = serde_json::from_slice(&payload)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    if !value.is_object() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame payload is not a json object",
        ));
    }
    Ok(Some(Message(value)))
}

// ---- message tags -----------------------------------------------------------

// Requests (host -> guest)
pub const T_HEALTH: &str = "health";
pub const T_EXEC: &str = "exec";
pub const T_STDIN: &str = "stdin";
pub const T_STDIN_CLOSE: &str = "stdin_close";
pub const T_RESIZE: &str = "resize";
pub const T_CANCEL: &str = "cancel";
pub const T_FS_READ: &str = "fs_read";
pub const T_FS_WRITE: &str = "fs_write";
pub const T_FS_STAT: &str = "fs_stat";
pub const T_FS_LIST: &str = "fs_list";
pub const T_FS_MKDIR: &str = "fs_mkdir";
pub const T_FS_DELETE: &str = "fs_delete";
pub const T_GIT: &str = "git";

// Responses / events (guest -> host)
pub const T_PONG: &str = "pong";
pub const T_STARTED: &str = "started";
pub const T_STDOUT: &str = "stdout";
pub const T_STDERR: &str = "stderr";
pub const T_EXIT: &str = "exit";
pub const T_OK: &str = "ok";
pub const T_ERROR: &str = "error";

/// Error categories mirrored from the Phase 0 API contract so the daemon can
/// surface failures the host maps straight back into `Error`.
pub const CAT_INVALID_ARGUMENT: &str = "invalid_argument";
pub const CAT_NOT_FOUND: &str = "not_found";
pub const CAT_FAILED_PRECONDITION: &str = "failed_precondition";
pub const CAT_INTERNAL: &str = "internal";
pub const CAT_IO: &str = "io";

// ---- base64 (standard alphabet, no external dependency) ---------------------

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as standard base64 with `=` padding.
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[(n >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[n as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Decode standard base64. Ignores ASCII whitespace; rejects other invalid
/// characters and malformed padding.
pub fn base64_decode(input: &str) -> Result<Vec<u8>, &'static str> {
    fn val(c: u8) -> Result<u32, &'static str> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err("invalid base64 character"),
        }
    }
    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if cleaned.len() % 4 != 0 {
        return Err("base64 length not a multiple of 4");
    }
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        let pad = chunk.iter().filter(|&&b| b == b'=').count();
        if pad > 2 {
            return Err("too much base64 padding");
        }
        // Padding may only appear at the end of the final chunk.
        if (chunk[0] == b'=' || chunk[1] == b'=')
            || (chunk[2] == b'=' && chunk[3] != b'=')
        {
            return Err("misplaced base64 padding");
        }
        let n = (val(chunk[0])? << 18)
            | (val(chunk[1])? << 12)
            | (if chunk[2] == b'=' { 0 } else { val(chunk[2])? } << 6)
            | (if chunk[3] == b'=' { 0 } else { val(chunk[3])? });
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_arbitrary_bytes() {
        for len in 0..=64usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let encoded = base64_encode(&bytes);
            let decoded = base64_decode(&encoded).expect("decode");
            assert_eq!(decoded, bytes, "len {len}");
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn base64_decode_rejects_garbage() {
        assert!(base64_decode("Zm9v!").is_err());
        assert!(base64_decode("Zg=").is_err()); // not multiple of 4
        assert!(base64_decode("Z===").is_err()); // too much padding
    }

    #[test]
    fn base64_decode_skips_whitespace() {
        assert_eq!(base64_decode("Zm9v\nYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn message_builders_and_accessors() {
        let msg = Message::new(T_EXEC, 7)
            .with_str("cwd", "/workspace")
            .with_bool("pty", true)
            .with_u64("rows", 24)
            .with_bytes("data", b"hi\x00there");
        assert_eq!(msg.t(), T_EXEC);
        assert_eq!(msg.id(), 7);
        assert_eq!(msg.str_field("cwd"), Some("/workspace"));
        assert!(msg.bool_field("pty"));
        assert_eq!(msg.u64_field("rows"), Some(24));
        assert_eq!(msg.bytes_field("data"), Some(b"hi\x00there".to_vec()));
        assert!(!msg.bool_field("absent"));
    }

    #[tokio::test]
    async fn frames_round_trip_over_a_pipe() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let sent = Message::new(T_STDOUT, 3).with_bytes("data", b"line\n");
        let sent2 = Message::new(T_EXIT, 3).with_i64("code", 0).with_bool("success", true);
        let writer = {
            let sent = sent.clone();
            let sent2 = sent2.clone();
            tokio::spawn(async move {
                write_message(&mut a, &sent).await.unwrap();
                write_message(&mut a, &sent2).await.unwrap();
                // Drop `a` to signal EOF.
            })
        };
        let got1 = read_message(&mut b).await.unwrap().unwrap();
        let got2 = read_message(&mut b).await.unwrap().unwrap();
        writer.await.unwrap();
        assert_eq!(got1, sent);
        assert_eq!(got2, sent2);
        let eof = read_message(&mut b).await.unwrap();
        assert!(eof.is_none());
    }

    #[tokio::test]
    async fn read_rejects_oversized_frame() {
        let (mut a, mut b) = tokio::io::duplex(16);
        tokio::spawn(async move {
            let _ = a.write_all(&u32::to_be_bytes(MAX_FRAME_LEN as u32 + 1)).await;
            let _ = a.flush().await;
            // Keep `a` alive so the reader hits the length check, not EOF.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            drop(a);
        });
        let err = read_message(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
