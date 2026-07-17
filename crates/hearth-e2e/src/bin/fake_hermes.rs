//! Deterministic stand-in for Hermes's pinned ACP JSON-RPC contract.

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

const SESSION_ID: &str = "fe9e3089-ccac-4609-b717-47f82bf41f81";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--version") {
        println!(
            "Hermes Agent v0.18.2 (fake) · upstream 4a69a662 · local 2ea39dae (+1 carried commit)"
        );
        return;
    }
    if args.iter().any(|arg| arg == "--check") {
        println!("Hermes ACP check passed");
        return;
    }
    if !args.iter().any(|arg| arg == "acp") {
        std::process::exit(2);
    }

    let stdin = io::stdin();
    let mut input = stdin.lock().lines();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut resumed = false;

    while let Some(Ok(line)) = input.next() {
        let message: Value = serde_json::from_str(&line).unwrap();
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match method {
            "initialize" => reply(
                &mut output,
                id,
                json!({
                    "protocolVersion": 1,
                    "agentInfo": { "name": "hermes-agent", "version": "0.18.2" },
                    "agentCapabilities": {
                        "loadSession": true,
                        "sessionCapabilities": { "resume": {} },
                    },
                }),
            ),
            "session/new" => {
                let mcp = &message["params"]["mcpServers"][0];
                assert_eq!(mcp["name"], json!("hearth"));
                assert_eq!(mcp["args"][0], json!("mcp"));
                assert_eq!(mcp["args"][1], json!("--thread"));
                resumed = false;
                reply(&mut output, id, json!({ "sessionId": SESSION_ID }));
            }
            "session/load" => {
                assert_eq!(message["params"]["sessionId"], json!(SESSION_ID));
                resumed = true;
                reply(&mut output, id, json!({}));
            }
            "session/prompt" => {
                let text = message["params"]["prompt"][0]["text"]
                    .as_str()
                    .unwrap_or_default();
                if text.contains("approval") {
                    permission(&mut output, SESSION_ID);
                    let response: Value =
                        serde_json::from_str(&input.next().unwrap().unwrap()).unwrap();
                    assert_eq!(response["id"], json!(700));
                    let choice = response["result"]["outcome"]["optionId"]
                        .as_str()
                        .unwrap_or("deny");
                    message_chunk(
                        &mut output,
                        SESSION_ID,
                        &format!("hermes approved: {choice}"),
                    );
                } else {
                    tool_round_trip(&mut output, SESSION_ID);
                    let prefix = if resumed {
                        "hermes resumed: "
                    } else {
                        "hermes echo: "
                    };
                    message_chunk(&mut output, SESSION_ID, &format!("{prefix}{text}"));
                }
                reply(
                    &mut output,
                    id,
                    json!({ "stopReason": "end_turn", "usage": { "totalTokens": 7 } }),
                );
            }
            _ => reply_error(&mut output, id, -32601, "method not found"),
        }
    }
}

fn send(output: &mut impl Write, value: Value) {
    writeln!(output, "{}", serde_json::to_string(&value).unwrap()).unwrap();
    output.flush().unwrap();
}

fn reply(output: &mut impl Write, id: Value, result: Value) {
    send(
        output,
        json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    );
}

fn reply_error(output: &mut impl Write, id: Value, code: i64, message: &str) {
    send(
        output,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    );
}

fn update(output: &mut impl Write, session_id: &str, body: Value) {
    send(
        output,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": { "sessionId": session_id, "update": body },
        }),
    );
}

fn message_chunk(output: &mut impl Write, session_id: &str, text: &str) {
    update(
        output,
        session_id,
        json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": text },
        }),
    );
}

fn tool_round_trip(output: &mut impl Write, session_id: &str) {
    update(
        output,
        session_id,
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-fake",
            "title": "read: /tmp/example",
            "rawInput": { "path": "/tmp/example" },
        }),
    );
    update(
        output,
        session_id,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tc-fake",
            "status": "completed",
            "content": [{
                "type": "content",
                "content": { "type": "text", "text": "example" },
            }],
        }),
    );
}

fn permission(output: &mut impl Write, session_id: &str) {
    send(
        output,
        json!({
            "jsonrpc": "2.0",
            "id": 700,
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "perm-check-1",
                    "title": "dangerous command: rm example",
                    "kind": "execute",
                    "status": "pending",
                },
                "options": [
                    { "optionId": "allow_once", "kind": "allow_once", "name": "Allow once" },
                    { "optionId": "deny", "kind": "reject_once", "name": "Deny" },
                ],
            },
        }),
    );
}
