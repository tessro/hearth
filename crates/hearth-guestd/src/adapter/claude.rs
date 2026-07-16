//! The claude adapter (docs/agent-plane.md §2.2, Phase 5 — the second CLI,
//! after the codex vertical proves the stack). Drives headless `claude -p` with
//! `stream-json` in and out and resumable sessions; permission prompts (its MCP
//! permission-prompt hook) map to task `awaiting_input`.
//!
//! As with codex, the wire shape below is a *pinned contract* modelled on
//! claude's stream-json events; it must be validated against a real claude CLI
//! before shipping — see docs/agent-plane-verification.md. The adapter refuses
//! a CLI version it does not pin (§2.2).

use super::{Adapter, AdapterEvent, RunOutput};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use hearth_agent_proto::events::AgentEvent;
use hearth_agent_proto::read_line_capped;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;

/// The single claude CLI version this adapter pins.
pub const PINNED_CLI_VERSION: &str = "1.0.0";

const LINE_CAP: usize = 4 * 1024 * 1024;

pub struct ClaudeAdapter {
    command: String,
}

impl ClaudeAdapter {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
        }
    }
}

#[async_trait]
impl Adapter for ClaudeAdapter {
    fn name(&self) -> &str {
        "claude"
    }

    async fn probe(&self) -> Result<String> {
        let output = Command::new(&self.command)
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .with_context(|| format!("probe {} --version", self.command))?;
        let text = String::from_utf8_lossy(&output.stdout);
        // Accept a version line that contains the pinned version token.
        if text.contains(PINNED_CLI_VERSION) {
            Ok(PINNED_CLI_VERSION.to_string())
        } else {
            bail!(
                "claude CLI version {:?} is not pinned ({:?}); rebuild the image with a matching \
                 claude or extend the adapter",
                text.trim(),
                PINNED_CLI_VERSION
            )
        }
    }

    async fn run(&self, native_thread: Option<&str>, input: &Value) -> Result<RunOutput> {
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut cmd = Command::new(&self.command);
        cmd.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--verbose");
        if let Some(session) = native_thread {
            cmd.arg("--resume").arg(session);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn {} -p", self.command))?;
        let mut stdin = child.stdin.take().context("claude stdin")?;
        let stdout = child.stdout.take().context("claude stdout")?;
        let mut reader = BufReader::new(stdout);

        // One user message, then close stdin so this run is a single turn.
        let user = json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        });
        stdin
            .write_all((serde_json::to_string(&user)? + "\n").as_bytes())
            .await?;
        stdin.shutdown().await?;
        drop(stdin);

        let mut events = Vec::new();
        let mut session_id = native_thread.map(str::to_string);
        loop {
            let Some(line) = read_line_capped(&mut reader, LINE_CAP).await? else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(msg) => msg,
                Err(_) => continue,
            };
            match translate(&msg, &mut session_id, &mut events) {
                Flow::Continue => {}
                Flow::Stop => break,
            }
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
        Ok(RunOutput {
            events,
            native_thread: session_id,
        })
    }
}

enum Flow {
    Continue,
    Stop,
}

fn translate(msg: &Value, session_id: &mut Option<String>, out: &mut Vec<AdapterEvent>) -> Flow {
    let kind = msg.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "system" => {
            if msg.get("subtype").and_then(Value::as_str) == Some("init") {
                if let Some(id) = msg.get("session_id").and_then(Value::as_str) {
                    *session_id = Some(id.to_string());
                }
            }
            Flow::Continue
        }
        "assistant" => {
            let content = msg
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array);
            if let Some(blocks) = content {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let delta = block.get("text").and_then(Value::as_str).unwrap_or_default();
                            out.push(AdapterEvent::Event(AgentEvent::TextMessageContent {
                                message_id: "claude-msg".to_string(),
                                delta: delta.to_string(),
                            }));
                        }
                        Some("tool_use") => {
                            let id = block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("claude-tool")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            out.push(AdapterEvent::Event(AgentEvent::ToolCallStart {
                                tool_call_id: id.clone(),
                                tool_call_name: name,
                                parent_message_id: None,
                            }));
                            if let Some(args) = block.get("input") {
                                out.push(AdapterEvent::Event(AgentEvent::ToolCallArgs {
                                    tool_call_id: id.clone(),
                                    delta: args.to_string(),
                                }));
                            }
                            out.push(AdapterEvent::Event(AgentEvent::ToolCallEnd {
                                tool_call_id: id,
                            }));
                        }
                        _ => {}
                    }
                }
            }
            Flow::Continue
        }
        // The MCP permission-prompt hook surfaces as a control request →
        // task awaiting_input (the run ends interrupted, §3.1).
        "control_request" | "permission_request" => {
            let prompt = msg
                .get("request")
                .cloned()
                .or_else(|| msg.get("permission").cloned())
                .unwrap_or_else(|| json!({ "kind": "permission", "raw": msg }));
            out.push(AdapterEvent::AwaitingInput {
                prompt: json!({ "kind": "permission", "request": prompt }),
            });
            Flow::Stop
        }
        "result" => {
            match msg.get("subtype").and_then(Value::as_str) {
                Some("success") | None => {
                    let result = msg
                        .get("result")
                        .cloned()
                        .unwrap_or_else(|| json!({ "summary": "" }));
                    out.push(AdapterEvent::Finished {
                        result: json!({ "summary": result }),
                    });
                }
                Some(other) => {
                    out.push(AdapterEvent::Failed {
                        message: format!("claude result {other}"),
                    });
                }
            }
            Flow::Stop
        }
        _ => {
            out.push(AdapterEvent::Event(AgentEvent::Raw {
                event: msg.clone(),
                source: Some("claude".to_string()),
            }));
            Flow::Continue
        }
    }
}
