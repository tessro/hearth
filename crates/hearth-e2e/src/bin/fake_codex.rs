//! A fake `codex app-server` that speaks the pinned JSON-RPC contract the
//! codex adapter drives (docs/agent-plane.md §2.2). Deterministic and
//! scriptable via the task text so the acceptance tests exercise every branch
//! (stream, tool call, interrupt→resume, failure) without a real codex binary.
//!
//! Scripting (matched in the first user turn's text):
//! - contains "NEEDS_APPROVAL" → stream a delta, then raise an exec approval
//!   (the run interrupts; the task goes awaiting_input).
//! - contains "FAIL" → turnFailed.
//! - otherwise → a text delta echoing the task, a shell tool call, turnComplete.
//!
//! On an approval resume (`respondApproval`) it streams a delta and completes.
//!
//! `HEARTH_FAKE_CODEX_VERSION` overrides the reported version so the adapter's
//! version-pin refusal (§2.2) is testable.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let version =
        std::env::var("HEARTH_FAKE_CODEX_VERSION").unwrap_or_else(|_| "0.1.0".to_string());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(_) => continue,
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        match method {
            "initialize" => {
                respond(
                    &mut stdout,
                    id,
                    json!({ "serverInfo": { "name": "codex", "version": version } }),
                );
            }
            "newThread" => {
                respond(&mut stdout, id, json!({ "threadId": "thread-1" }));
            }
            "resumeThread" => {
                respond(&mut stdout, id, json!({}));
            }
            "sendUserTurn" => {
                let text = params
                    .get("input")
                    .and_then(|i| i.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                drive_turn(&mut stdout, &text);
            }
            "respondApproval" => {
                // The approval was answered: finish the turn.
                emit(
                    &mut stdout,
                    json!({ "method": "item", "params": { "threadId": "thread-1",
                        "item": { "type": "agentMessageDelta", "messageId": "m2",
                                  "delta": "approved; finishing." } } }),
                );
                emit(
                    &mut stdout,
                    json!({ "method": "turnComplete", "params": {
                        "threadId": "thread-1",
                        "result": { "summary": "done after approval" } } }),
                );
            }
            _ => {}
        }
    }
}

fn drive_turn(stdout: &mut std::io::Stdout, text: &str) {
    emit(
        stdout,
        json!({ "method": "item", "params": { "threadId": "thread-1",
            "item": { "type": "agentMessageDelta", "messageId": "m1",
                      "delta": format!("echo: {text}") } } }),
    );
    if text.contains("FAIL") {
        emit(
            stdout,
            json!({ "method": "turnFailed", "params": {
                "threadId": "thread-1", "error": "scripted failure" } }),
        );
        return;
    }
    if text.contains("NEEDS_APPROVAL") {
        emit(
            stdout,
            json!({ "id": 100, "method": "execApproval", "params": {
                "threadId": "thread-1",
                "call": { "command": ["rm", "-rf", "/tmp/scratch"] } } }),
        );
        return;
    }
    // A normal tool call, then completion.
    emit(
        stdout,
        json!({ "method": "item", "params": { "threadId": "thread-1",
            "item": { "type": "commandExecutionBegin", "callId": "c1",
                      "command": ["echo", "hi"] } } }),
    );
    emit(
        stdout,
        json!({ "method": "item", "params": { "threadId": "thread-1",
            "item": { "type": "commandExecutionEnd", "callId": "c1",
                      "output": "hi\n" } } }),
    );
    emit(
        stdout,
        json!({ "method": "turnComplete", "params": {
            "threadId": "thread-1",
            "result": { "summary": format!("completed: {text}") } } }),
    );
}

fn respond(stdout: &mut std::io::Stdout, id: Option<Value>, result: Value) {
    emit(
        stdout,
        json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    );
}

fn emit(stdout: &mut std::io::Stdout, value: Value) {
    let line = serde_json::to_string(&value).unwrap();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}
