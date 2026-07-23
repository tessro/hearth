//! The claude adapter (docs/agent-plane.md §2.2, Phase 5 — the second CLI,
//! after the codex vertical proves the stack). Drives headless `claude -p` with
//! `stream-json` in and out and resumable sessions; permission prompts (its MCP
//! permission-prompt hook) map to task `awaiting_input`.
//!
//! As with codex, the wire shape below is a *pinned contract* modelled on
//! claude's stream-json events; it must be validated against a real claude CLI
//! before shipping — see ARCHITECTURE.md §Known follow-ups. The adapter refuses
//! a CLI version it does not pin (§2.2).

use super::{flush_events, Adapter, AdapterEvent, EventSink, RunOutput};
use crate::store::new_ulid;
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

    async fn run(
        &self,
        _thread_id: &str,
        native_thread: Option<&str>,
        input: &Value,
        event_sink: EventSink,
    ) -> Result<RunOutput> {
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

        let mut translated = Translation::default();
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
            let flow = translated.update(&msg, &mut session_id);
            flush_events(&mut translated.events, &event_sink)?;
            match flow {
                Flow::Continue => {}
                Flow::Stop => break,
            }
        }
        translated.close_message();
        flush_events(&mut translated.events, &event_sink)?;
        let _ = child.start_kill();
        let _ = child.wait().await;
        Ok(RunOutput {
            events: translated.events,
            native_thread: session_id,
        })
    }
}

enum Flow {
    Continue,
    Stop,
}

struct Translation {
    events: Vec<AdapterEvent>,
    turn_id: String,
    message_id: Option<String>,
    message_count: u64,
    tool_count: u64,
}

impl Default for Translation {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            turn_id: new_ulid(),
            message_id: None,
            message_count: 0,
            tool_count: 0,
        }
    }
}

impl Translation {
    fn update(&mut self, msg: &Value, session_id: &mut Option<String>) -> Flow {
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
                                let delta = block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                self.text(delta);
                            }
                            Some("tool_use") => {
                                self.close_message();
                                let id =
                                    format!("claude-tool-{}-{}", self.turn_id, self.tool_count);
                                self.tool_count += 1;
                                let name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool")
                                    .to_string();
                                self.events
                                    .push(AdapterEvent::Event(AgentEvent::ToolCallStart {
                                        tool_call_id: id.clone(),
                                        tool_call_name: name,
                                        parent_message_id: None,
                                    }));
                                if let Some(args) = block.get("input") {
                                    self.events.push(AdapterEvent::Event(
                                        AgentEvent::ToolCallArgs {
                                            tool_call_id: id.clone(),
                                            delta: args.to_string(),
                                        },
                                    ));
                                }
                                self.events
                                    .push(AdapterEvent::Event(AgentEvent::ToolCallEnd {
                                        tool_call_id: id,
                                    }));
                            }
                            _ => {}
                        }
                    }
                }
                self.close_message();
                Flow::Continue
            }
            // The MCP permission-prompt hook surfaces as a control request →
            // task awaiting_input (the run ends interrupted, §3.1).
            "control_request" | "permission_request" => {
                self.close_message();
                let prompt = msg
                    .get("request")
                    .cloned()
                    .or_else(|| msg.get("permission").cloned())
                    .unwrap_or_else(|| json!({ "kind": "permission", "raw": msg }));
                self.events.push(AdapterEvent::AwaitingInput {
                    prompt: json!({ "kind": "permission", "request": prompt }),
                });
                Flow::Stop
            }
            "result" => {
                self.close_message();
                match msg.get("subtype").and_then(Value::as_str) {
                    Some("success") | None => {
                        let result = msg
                            .get("result")
                            .cloned()
                            .unwrap_or_else(|| json!({ "summary": "" }));
                        self.events.push(AdapterEvent::Finished {
                            result: json!({ "summary": result }),
                        });
                    }
                    Some(other) => {
                        self.events.push(AdapterEvent::Failed {
                            message: format!("claude result {other}"),
                        });
                    }
                }
                Flow::Stop
            }
            _ => {
                self.events.push(AdapterEvent::Event(AgentEvent::Raw {
                    event: msg.clone(),
                    source: Some("claude".to_string()),
                }));
                Flow::Continue
            }
        }
    }

    fn text(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if self.message_id.is_none() {
            let message_id = format!("claude-message-{}-{}", self.turn_id, self.message_count);
            self.message_count += 1;
            self.events
                .push(AdapterEvent::Event(AgentEvent::TextMessageStart {
                    message_id: message_id.clone(),
                    role: "assistant".to_string(),
                }));
            self.message_id = Some(message_id);
        }
        self.events
            .push(AdapterEvent::Event(AgentEvent::TextMessageContent {
                message_id: self.message_id.clone().expect("message just opened"),
                delta: delta.to_string(),
            }));
    }

    fn close_message(&mut self) {
        if let Some(message_id) = self.message_id.take() {
            self.events
                .push(AdapterEvent::Event(AgentEvent::TextMessageEnd {
                    message_id,
                }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_tools_are_complete_unique_agui_segments() {
        let mut translated = Translation::default();
        let mut session = None;
        translated.update(
            &json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "text", "text": "before" },
                    { "type": "tool_use", "name": "Bash", "input": { "command": "true" } },
                    { "type": "text", "text": "after" }
                ] }
            }),
            &mut session,
        );
        let starts: Vec<&str> = translated
            .events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::Event(AgentEvent::TextMessageStart { message_id, .. }) => {
                    Some(message_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 2);
        assert_ne!(starts[0], starts[1]);
        assert_eq!(
            translated
                .events
                .iter()
                .filter(|event| matches!(
                    event,
                    AdapterEvent::Event(AgentEvent::TextMessageEnd { .. })
                ))
                .count(),
            2
        );
    }
}
