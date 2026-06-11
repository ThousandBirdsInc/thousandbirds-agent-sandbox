//! Git operations, run through the guest's own `git` binary. The request
//! carries the repository path and a pre-built `git` argv; the daemon runs
//! `git -C <path> <argv...>` and returns the captured result. Keeping the argv
//! host-built means new git verbs need no daemon change.

use agent_sandbox_protocol::{Message, CAT_INVALID_ARGUMENT, T_ERROR, T_OK};
use serde_json::Value;

/// Run a git command and return the response message (async: git can block on
/// network for clone/pull/push).
pub async fn handle(req: &Message) -> Message {
    let id = req.id();
    let path = req.str_field("path").unwrap_or(".").to_string();
    let argv: Vec<String> = match req.value_field("argv") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => return err(id, CAT_INVALID_ARGUMENT, "git request requires an argv array"),
    };
    if argv.is_empty() {
        return err(id, CAT_INVALID_ARGUMENT, "git argv must be non-empty");
    }

    let mut command = tokio::process::Command::new("git");
    command.arg("-C").arg(&path).args(&argv);
    command.env("GIT_TERMINAL_PROMPT", "0");
    match command.output().await {
        Ok(out) => Message::new(T_OK, id)
            .with_i64("code", out.status.code().unwrap_or(-1) as i64)
            .with_bool("success", out.status.success())
            .with_bytes("stdout", &out.stdout)
            .with_bytes("stderr", &out.stderr),
        Err(e) => err(id, agent_sandbox_protocol::CAT_INTERNAL, &format!("git spawn failed: {e}")),
    }
}

fn err(id: u64, category: &str, message: &str) -> Message {
    Message::new(T_ERROR, id)
        .with_str("category", category)
        .with_str("message", message)
}
