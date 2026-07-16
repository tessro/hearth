//! A fake `claude -p --output-format stream-json` for the Phase 5 adapter
//! (docs/agent-plane.md §2.2). Deterministic and scriptable by the user
//! message text, mirroring claude's stream-json event shape.
//!
//! - `--version` prints the pinned version so the adapter's probe passes.
//! - Otherwise reads one stream-json user message on stdin and emits:
//!   system/init (session id), an assistant text (+ tool_use for a normal
//!   turn), then a `result`. Text containing "NEEDS_APPROVAL" emits a
//!   `permission_request` instead (interrupt → awaiting_input). "FAIL" →
//!   an error result. A `--resume` invocation completes the turn.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("claude 1.0.0 (fake)");
        return;
    }
    let resuming = args.iter().any(|a| a == "--resume");

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut text = String::new();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(msg) = serde_json::from_str::<Value>(&line) {
            if msg.get("type").and_then(Value::as_str) == Some("user") {
                text = msg
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                break;
            }
        }
    }

    emit(
        &mut stdout,
        json!({ "type": "system", "subtype": "init", "session_id": "sess-1" }),
    );

    if resuming {
        emit(
            &mut stdout,
            json!({ "type": "assistant",
                "message": { "role": "assistant",
                    "content": [{ "type": "text", "text": "approved; finishing." }] } }),
        );
        emit(
            &mut stdout,
            json!({ "type": "result", "subtype": "success", "result": "done after approval" }),
        );
        return;
    }

    emit(
        &mut stdout,
        json!({ "type": "assistant",
            "message": { "role": "assistant",
                "content": [{ "type": "text", "text": format!("claude echo: {text}") }] } }),
    );

    if text.contains("FAIL") {
        emit(
            &mut stdout,
            json!({ "type": "result", "subtype": "error_max_turns" }),
        );
        return;
    }
    if text.contains("NEEDS_APPROVAL") {
        emit(
            &mut stdout,
            json!({ "type": "permission_request",
                "request": { "tool": "Bash", "command": "rm -rf /tmp/x" } }),
        );
        return;
    }
    emit(
        &mut stdout,
        json!({ "type": "assistant",
            "message": { "role": "assistant",
                "content": [{ "type": "tool_use", "id": "tu1", "name": "Bash",
                              "input": { "command": "echo hi" } }] } }),
    );
    emit(
        &mut stdout,
        json!({ "type": "result", "subtype": "success",
            "result": format!("claude completed: {text}") }),
    );
}

fn emit(stdout: &mut std::io::Stdout, value: Value) {
    let _ = writeln!(stdout, "{}", serde_json::to_string(&value).unwrap());
    let _ = stdout.flush();
}
