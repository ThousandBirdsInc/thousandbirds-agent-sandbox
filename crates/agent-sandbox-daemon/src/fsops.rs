//! First-class filesystem operations. These replace the line-oriented
//! `cat`/`printf`-over-exec bridge the request-file transport relied on, so
//! reads and writes are binary-safe.

use std::path::Path;

use agent_sandbox_protocol::{
    Message, CAT_INVALID_ARGUMENT, CAT_IO, CAT_NOT_FOUND, T_OK,
};
use serde_json::{json, Value};

/// Dispatch an `fs_*` request, returning the response message to send back.
pub fn handle(req: &Message) -> Message {
    let id = req.id();
    let path = match req.str_field("path") {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return err(id, CAT_INVALID_ARGUMENT, "fs request requires a non-empty path"),
    };
    let p = Path::new(&path);
    match req.t() {
        agent_sandbox_protocol::T_FS_READ => read(id, p),
        agent_sandbox_protocol::T_FS_WRITE => write(id, p, req),
        agent_sandbox_protocol::T_FS_STAT => stat(id, p),
        agent_sandbox_protocol::T_FS_LIST => list(id, p),
        agent_sandbox_protocol::T_FS_MKDIR => mkdir(id, p, req.bool_field("parents")),
        agent_sandbox_protocol::T_FS_DELETE => delete(id, p, req.bool_field("recursive")),
        other => err(id, CAT_INVALID_ARGUMENT, &format!("unknown fs op {other}")),
    }
}

fn read(id: u64, path: &Path) -> Message {
    match std::fs::read(path) {
        Ok(bytes) => Message::new(T_OK, id).with_bytes("data", &bytes),
        Err(e) => io_err(id, path, e),
    }
}

fn write(id: u64, path: &Path, req: &Message) -> Message {
    let data = req.bytes_field("data").unwrap_or_default();
    let mode = req.str_field("mode").unwrap_or("overwrite");
    let result = match mode {
        "append" => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, &data)),
        "create" => std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, &data)),
        _ => std::fs::write(path, &data),
    };
    match result {
        Ok(_) => Message::new(T_OK, id).with_u64("bytes", data.len() as u64),
        Err(e) => io_err(id, path, e),
    }
}

fn stat(id: u64, path: &Path) -> Message {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Message::new(T_OK, id).with_value("stat", stat_value(path, &meta)),
        Err(e) => io_err(id, path, e),
    }
}

fn list(id: u64, path: &Path) -> Message {
    let entries = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => return io_err(id, path, e),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            out.push(stat_value(&entry.path(), &meta));
        }
    }
    Message::new(T_OK, id).with_value("entries", Value::Array(out))
}

fn mkdir(id: u64, path: &Path, parents: bool) -> Message {
    let result = if parents {
        std::fs::create_dir_all(path)
    } else {
        std::fs::create_dir(path)
    };
    match result {
        Ok(_) => Message::new(T_OK, id),
        Err(e) => io_err(id, path, e),
    }
}

fn delete(id: u64, path: &Path, recursive: bool) -> Message {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => return io_err(id, path, e),
    };
    let result = if meta.is_dir() {
        if recursive {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_dir(path)
        }
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(_) => Message::new(T_OK, id),
        Err(e) => io_err(id, path, e),
    }
}

fn stat_value(path: &Path, meta: &std::fs::Metadata) -> Value {
    use std::os::unix::fs::MetadataExt;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    json!({
        "name": name,
        "path": path.to_string_lossy(),
        "is_dir": meta.is_dir(),
        "is_file": meta.is_file(),
        "is_symlink": meta.file_type().is_symlink(),
        "size": meta.len(),
        "mode": meta.mode(),
    })
}

fn io_err(id: u64, path: &Path, e: std::io::Error) -> Message {
    let category = if e.kind() == std::io::ErrorKind::NotFound {
        CAT_NOT_FOUND
    } else {
        CAT_IO
    };
    err(id, category, &format!("{}: {e}", path.display()))
}

fn err(id: u64, category: &str, message: &str) -> Message {
    Message::new(agent_sandbox_protocol::T_ERROR, id)
        .with_str("category", category)
        .with_str("message", message)
}
