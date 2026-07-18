//! The codex adapter (docs/agent-plane.md §2.2, the first vertical). Drives
//! `codex app-server`: newline-delimited JSON-RPC 2.0 over stdio, with
//! threads, turns, streamed items, and server-initiated approval requests.
//!
//! The exact wire method/item names below are a *pinned contract* for
//! `PINNED_APP_SERVER_VERSION`. They are modelled on codex app-server's shape
//! (JSON-RPC, `newThread`/`resumeThread`/`sendUserTurn`, streamed `item`
//! notifications, an `execApproval` server request) but must be validated
//! against a real codex binary before shipping — see
//! docs/agent-plane-verification.md. The adapter refuses (loudly, at boot
//! report) any app-server whose reported version it does not pin, exactly as
//! §2.2 requires; that refusal is what makes the pin enforceable rather than
//! aspirational.

use super::{flush_events, Adapter, AdapterEvent, EventSink, RunOutput};
use crate::store::new_ulid;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use hearth_agent_proto::events::AgentEvent;
use hearth_agent_proto::read_line_capped;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// The single app-server version this adapter knows how to translate.
pub const PINNED_APP_SERVER_VERSION: &str = "0.1.0";

const LINE_CAP: usize = 4 * 1024 * 1024;

pub struct CodexAdapter {
    /// The codex binary (`codex` in the image; overridable for tests).
    command: String,
}

impl CodexAdapter {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
        }
    }

    async fn spawn(&self) -> Result<AppServer> {
        let mut child = Command::new(&self.command)
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn {} app-server", self.command))?;
        let stdin = child.stdin.take().context("app-server stdin")?;
        let stdout = child.stdout.take().context("app-server stdout")?;
        let mut server = AppServer {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };
        let init = server
            .request(
                "initialize",
                json!({ "clientInfo": { "name": "hearth-guestd" } }),
            )
            .await?;
        let version = init
            .get("serverInfo")
            .and_then(|s| s.get("version"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if version != PINNED_APP_SERVER_VERSION {
            bail!(
                "codex app-server version {version:?} is not pinned ({:?}); rebuild the image \
                 with a matching codex or extend the adapter",
                PINNED_APP_SERVER_VERSION
            );
        }
        Ok(server)
    }
}

#[async_trait]
impl Adapter for CodexAdapter {
    fn name(&self) -> &str {
        "codex"
    }

    async fn probe(&self) -> Result<String> {
        let mut server = self.spawn().await?;
        server.shutdown().await;
        Ok(PINNED_APP_SERVER_VERSION.to_string())
    }

    async fn run(
        &self,
        _thread_id: &str,
        native_thread: Option<&str>,
        input: &Value,
        events: EventSink,
    ) -> Result<RunOutput> {
        let mut server = self.spawn().await?;
        let thread_id = match native_thread {
            None => {
                let created = server.request("newThread", json!({})).await?;
                created
                    .get("threadId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("newThread returned no threadId"))?
                    .to_string()
            }
            Some(existing) => {
                server
                    .request("resumeThread", json!({ "threadId": existing }))
                    .await?;
                existing.to_string()
            }
        };

        // A resume payload answering an approval carries `{approval:{id,decision}}`;
        // otherwise the input is a fresh user turn.
        if let Some(approval) = input.get("approval") {
            server
                .notify(
                    "respondApproval",
                    json!({ "threadId": thread_id, "approval": approval }),
                )
                .await?;
        } else {
            server
                .notify(
                    "sendUserTurn",
                    json!({ "threadId": thread_id, "input": input }),
                )
                .await?;
        }

        let terminal_events = server.drive_turn(&thread_id, &events).await?;
        server.shutdown().await;
        Ok(RunOutput {
            events: terminal_events,
            native_thread: Some(thread_id),
        })
    }
}

struct AppServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl AppServer {
    async fn send(&mut self, value: &Value) -> Result<()> {
        let line = serde_json::to_string(value)? + "\n";
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Value> {
        let line = read_line_capped(&mut self.stdout, LINE_CAP)
            .await?
            .ok_or_else(|| anyhow!("codex app-server closed the stream"))?;
        serde_json::from_str(&line).context("parse app-server line")
    }

    /// One request/response round-trip. Streamed `item`/approval notifications
    /// that arrive before the matching response are not expected here (they
    /// only flow during a turn), so a stray notification is an error.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        loop {
            let msg = self.recv().await?;
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = msg.get("error") {
                    bail!("app-server {method} error: {err}");
                }
                return Ok(msg.get("result").cloned().unwrap_or(json!({})));
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// Read streamed items until the turn ends: `turnComplete`, `turnFailed`,
    /// or an `execApproval` server request (which interrupts).
    async fn drive_turn(
        &mut self,
        thread_id: &str,
        events: &EventSink,
    ) -> Result<Vec<AdapterEvent>> {
        let mut out = Vec::new();
        let mut translated = Translation::default();
        loop {
            let msg = self.recv().await?;
            let method = msg
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            match method {
                "item" => {
                    if let Some(item) = params.get("item") {
                        translated.update(item, &mut out);
                        flush_events(&mut out, events)?;
                    }
                }
                "execApproval" => {
                    translated.close_message(&mut out);
                    flush_events(&mut out, events)?;
                    // Server-initiated approval → task awaiting_input. The run
                    // ends interrupted; answering starts a new run (§3.1).
                    let call = params.get("call").cloned().unwrap_or(json!({}));
                    let approval_id = msg
                        .get("id")
                        .cloned()
                        .or_else(|| params.get("id").cloned())
                        .unwrap_or(Value::Null);
                    out.push(AdapterEvent::AwaitingInput {
                        prompt: json!({
                            "kind": "exec_approval",
                            "approval_id": approval_id,
                            "call": call,
                            "thread_id": thread_id,
                        }),
                    });
                    return Ok(out);
                }
                "turnComplete" => {
                    translated.close_message(&mut out);
                    flush_events(&mut out, events)?;
                    let result = params
                        .get("result")
                        .cloned()
                        .unwrap_or_else(|| json!({ "summary": translated.summary }));
                    out.push(AdapterEvent::Finished { result });
                    return Ok(out);
                }
                "turnFailed" => {
                    translated.close_message(&mut out);
                    flush_events(&mut out, events)?;
                    let message = params
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("codex turn failed")
                        .to_string();
                    out.push(AdapterEvent::Failed { message });
                    return Ok(out);
                }
                _ => {
                    // Unknown item is preserved as RAW so nothing is silently
                    // dropped (the §5.1 mapping for CLI-specific extras).
                    out.push(AdapterEvent::Event(AgentEvent::Raw {
                        event: msg,
                        source: Some("codex".to_string()),
                    }));
                    flush_events(&mut out, events)?;
                }
            }
        }
    }

    async fn shutdown(&mut self) {
        let _ = self.stdin.shutdown().await;
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

struct Translation {
    turn_id: String,
    message_count: u64,
    message_id: Option<String>,
    provider_message_id: Option<String>,
    tool_count: u64,
    tool_ids: HashMap<String, String>,
    summary: String,
}

impl Default for Translation {
    fn default() -> Self {
        Self {
            turn_id: new_ulid(),
            message_count: 0,
            message_id: None,
            provider_message_id: None,
            tool_count: 0,
            tool_ids: HashMap::new(),
            summary: String::new(),
        }
    }
}

impl Translation {
    /// Translate one codex streamed item into canonical AG-UI events (§5.1).
    fn update(&mut self, item: &Value, out: &mut Vec<AdapterEvent>) {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        match item_type {
            "agentMessageDelta" => {
                let Some(provider_message_id) = item.get("messageId").and_then(Value::as_str)
                else {
                    self.raw(item, out);
                    return;
                };
                let Some(delta) = item.get("delta").and_then(Value::as_str) else {
                    self.raw(item, out);
                    return;
                };
                if delta.is_empty() {
                    return;
                }
                if self.provider_message_id.as_deref() != Some(provider_message_id) {
                    self.close_message(out);
                    let message_id =
                        format!("codex-message-{}-{}", self.turn_id, self.message_count);
                    self.message_count += 1;
                    out.push(AdapterEvent::Event(AgentEvent::TextMessageStart {
                        message_id: message_id.clone(),
                        role: "assistant".to_string(),
                    }));
                    self.provider_message_id = Some(provider_message_id.to_string());
                    self.message_id = Some(message_id);
                }
                self.summary.push_str(delta);
                out.push(AdapterEvent::Event(AgentEvent::TextMessageContent {
                    message_id: self.message_id.clone().expect("message just opened"),
                    delta: delta.to_string(),
                }));
            }
            "commandExecutionBegin" => {
                self.close_message(out);
                let Some(provider_id) = item.get("callId").and_then(Value::as_str) else {
                    self.raw(item, out);
                    return;
                };
                let tool_call_id = format!("codex-tool-{}-{}", self.turn_id, self.tool_count);
                self.tool_count += 1;
                self.tool_ids
                    .insert(provider_id.to_string(), tool_call_id.clone());
                out.push(AdapterEvent::Event(AgentEvent::ToolCallStart {
                    tool_call_id: tool_call_id.clone(),
                    tool_call_name: "shell".to_string(),
                    parent_message_id: None,
                }));
                if let Some(command) = item.get("command") {
                    out.push(AdapterEvent::Event(AgentEvent::ToolCallArgs {
                        tool_call_id,
                        delta: command.to_string(),
                    }));
                }
            }
            "commandExecutionEnd" => {
                let Some(provider_id) = item.get("callId").and_then(Value::as_str) else {
                    self.raw(item, out);
                    return;
                };
                let Some(tool_call_id) = self.tool_ids.remove(provider_id) else {
                    self.raw(item, out);
                    return;
                };
                out.push(AdapterEvent::Event(AgentEvent::ToolCallEnd {
                    tool_call_id: tool_call_id.clone(),
                }));
                let content = item
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                out.push(AdapterEvent::Event(AgentEvent::ToolCallResult {
                    message_id: format!("{tool_call_id}-result"),
                    tool_call_id,
                    content,
                    role: Some("tool".to_string()),
                }));
            }
            _ => self.raw(item, out),
        }
    }

    fn close_message(&mut self, out: &mut Vec<AdapterEvent>) {
        if let Some(message_id) = self.message_id.take() {
            out.push(AdapterEvent::Event(AgentEvent::TextMessageEnd {
                message_id,
            }));
        }
        self.provider_message_id = None;
    }

    fn raw(&self, item: &Value, out: &mut Vec<AdapterEvent>) {
        out.push(AdapterEvent::Event(AgentEvent::Raw {
            event: item.clone(),
            source: Some("codex".to_string()),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_tools_are_complete_unique_agui_segments() {
        let mut translated = Translation::default();
        let mut out = Vec::new();
        translated.update(
            &json!({ "type": "agentMessageDelta", "messageId": "m", "delta": "before" }),
            &mut out,
        );
        translated.update(
            &json!({ "type": "commandExecutionBegin", "callId": "c", "command": ["true"] }),
            &mut out,
        );
        translated.update(
            &json!({ "type": "commandExecutionEnd", "callId": "c", "output": "" }),
            &mut out,
        );
        translated.update(
            &json!({ "type": "agentMessageDelta", "messageId": "m", "delta": "after" }),
            &mut out,
        );
        translated.close_message(&mut out);

        let starts: Vec<&str> = out
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
            out.iter()
                .filter(|event| matches!(
                    event,
                    AdapterEvent::Event(AgentEvent::TextMessageEnd { .. })
                ))
                .count(),
            2
        );
    }
}
