//! Hermes adapter (docs/agent-plane.md §2.2). Drives Hermes's pinned Agent
//! Client Protocol (ACP) server over newline-delimited JSON-RPC stdio. ACP
//! supplies native sessions, streamed message/tool updates, cancellation, and
//! server-initiated permission requests; Hearth maps those messages onto its
//! task/thread/run model and AG-UI event vocabulary without parsing terminal
//! presentation text.

use super::{flush_events, Adapter, AdapterEvent, EventSink, RunOutput};
use crate::store::new_ulid;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use hearth_agent_proto::events::AgentEvent;
use hearth_agent_proto::read_line_capped;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// Hermes release and checked-out source revision validated against a real CLI.
pub const PINNED_CLI_VERSION: &str = "0.18.2";
pub const PINNED_SOURCE_COMMIT: &str = "2ea39dae";
const PINNED_ACP_PROTOCOL: u64 = 1;

const LINE_CAP: usize = 4 * 1024 * 1024;
const OUTPUT_CAP: u64 = 4 * 1024 * 1024;
const REASONING_ID: &str = "hermes-reasoning";
const DEFAULT_MCP_COMMAND: &str = "/usr/local/bin/hearth-guestd";

/// Optional process identity for a CLI installed and authenticated as the
/// image's unprivileged workload user. Tests leave this unset.
#[derive(Debug, Clone)]
pub struct ProcessIdentity {
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

pub struct HermesAdapter {
    command: String,
    identity: Option<ProcessIdentity>,
    mcp_command: String,
    /// An ACP prompt remains live while Hermes waits on a permission request.
    /// Park that exact process here so task.respond answers the server request
    /// instead of trying to reconstruct it in a new process.
    pending: Mutex<HashMap<String, PendingAcp>>,
}

impl HermesAdapter {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            identity: None,
            mcp_command: DEFAULT_MCP_COMMAND.to_string(),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn run_as(mut self, identity: ProcessIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    pub fn with_mcp_command(mut self, command: impl Into<String>) -> Self {
        self.mcp_command = command.into();
        self
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.command);
        cmd.kill_on_drop(true);
        if let Some(identity) = &self.identity {
            // Hermes is an external agent boundary. Give it only the workload
            // user's ordinary environment; it loads its own provider config
            // from HOME. Hearth thread/session values are passed as protocol
            // fields, not interpolated into a shell.
            cmd.env_clear()
                .uid(identity.uid)
                .gid(identity.gid)
                .current_dir(&identity.home)
                .env("HOME", &identity.home)
                .env("USER", "agent")
                .env("LOGNAME", "agent")
                .env(
                    "PATH",
                    "/home/agent/.local/bin:/home/agent/.cargo/bin:/usr/local/sbin:\
                     /usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                )
                .env("XDG_RUNTIME_DIR", format!("/run/user/{}", identity.uid));
        }
        cmd
    }

    async fn output(&self, args: &[&str]) -> Result<ProcessOutput> {
        let mut cmd = self.command();
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {}", self.command))?;
        let stdout = child.stdout.take().context("Hermes stdout")?;
        let stderr = child.stderr.take().context("Hermes stderr")?;
        let (stdout, stderr) = tokio::try_join!(read_capped(stdout), read_capped(stderr))?;
        if stdout.truncated || stderr.truncated {
            let _ = child.start_kill();
            let _ = child.wait().await;
            bail!("Hermes output exceeded {OUTPUT_CAP} bytes");
        }
        let status = child.wait().await.context("wait for Hermes")?;
        Ok(ProcessOutput {
            success: status.success(),
            stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
            stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        })
    }

    async fn spawn_acp(&self) -> Result<AcpServer> {
        let mut cmd = self.command();
        cmd.arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // ACP logs belong in guestd's journal, while stdout remains a
            // protocol-only stream.
            .stderr(Stdio::inherit());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {} acp", self.command))?;
        let stdin = child.stdin.take().context("Hermes ACP stdin")?;
        let stdout = child.stdout.take().context("Hermes ACP stdout")?;
        let mut server = AcpServer {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };
        let initialized = server
            .request(
                "initialize",
                json!({
                    "protocolVersion": PINNED_ACP_PROTOCOL,
                    "clientCapabilities": {},
                    "clientInfo": {
                        "name": "hearth-guestd",
                        "version": crate::VERSION,
                    },
                }),
            )
            .await
            .context("initialize Hermes ACP")?;
        let protocol = initialized.get("protocolVersion").and_then(Value::as_u64);
        let name = initialized
            .get("agentInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str);
        let version = initialized
            .get("agentInfo")
            .and_then(|info| info.get("version"))
            .and_then(Value::as_str);
        if protocol != Some(PINNED_ACP_PROTOCOL)
            || name != Some("hermes-agent")
            || version != Some(PINNED_CLI_VERSION)
        {
            server.shutdown().await;
            bail!(
                "Hermes ACP mismatch: protocol={protocol:?}, agent={name:?}, version={version:?}"
            );
        }
        Ok(server)
    }

    fn mcp_servers(&self, thread_id: &str) -> Value {
        json!([{
            "name": "hearth",
            "command": self.mcp_command,
            "args": ["mcp", "--thread", thread_id],
            "env": [],
        }])
    }

    async fn drive_new_turn(
        &self,
        thread_id: &str,
        native_thread: Option<&str>,
        input: &Value,
        events: &EventSink,
    ) -> Result<RunOutput> {
        let text = input_text(input)?;
        let mut server = self.spawn_acp().await?;
        let session_id = match native_thread {
            Some(session_id) => {
                validate_session_id(session_id)?;
                server
                    .request(
                        "session/load",
                        json!({
                            "cwd": self.cwd(),
                            "sessionId": session_id,
                            "mcpServers": self.mcp_servers(thread_id),
                        }),
                    )
                    .await
                    .with_context(|| format!("load Hermes ACP session {session_id}"))?;
                session_id.to_string()
            }
            None => {
                let created = server
                    .request(
                        "session/new",
                        json!({
                            "cwd": self.cwd(),
                            "mcpServers": self.mcp_servers(thread_id),
                        }),
                    )
                    .await
                    .context("create Hermes ACP session")?;
                let session_id = created
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .context("Hermes session/new returned no sessionId")?
                    .to_string();
                validate_session_id(&session_id)?;
                session_id
            }
        };
        let prompt_id = server
            .begin_request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": text }],
                }),
            )
            .await?;
        let driven = server.drive_prompt(&session_id, &prompt_id, events).await;
        match driven {
            Ok(driven) => {
                self.finish_or_park(server, session_id, prompt_id, driven)
                    .await
            }
            Err(err) => {
                server.shutdown().await;
                Err(err)
            }
        }
    }

    async fn resume_permission(
        &self,
        session_id: &str,
        input: &Value,
        mut pending: PendingAcp,
        events: &EventSink,
    ) -> Result<RunOutput> {
        let decision = match permission_decision(input, &pending.permission.options) {
            Ok(decision) => decision,
            Err(err) => {
                let mut prompt = pending.permission.prompt.clone();
                prompt["validation_error"] = json!(err.to_string());
                let old = self
                    .pending
                    .lock()
                    .await
                    .insert(session_id.to_string(), pending);
                if let Some(old) = old {
                    old.server.shutdown().await;
                }
                return Ok(RunOutput {
                    events: vec![AdapterEvent::AwaitingInput { prompt }],
                    native_thread: Some(session_id.to_string()),
                });
            }
        };
        pending
            .server
            .send(&json!({
                "jsonrpc": "2.0",
                "id": pending.permission.request_id,
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": decision,
                    }
                }
            }))
            .await
            .context("answer Hermes ACP permission request")?;
        let driven = pending
            .server
            .drive_prompt(session_id, &pending.prompt_id, events)
            .await;
        match driven {
            Ok(driven) => {
                self.finish_or_park(
                    pending.server,
                    session_id.to_string(),
                    pending.prompt_id,
                    driven,
                )
                .await
            }
            Err(err) => {
                pending.server.shutdown().await;
                Err(err)
            }
        }
    }

    async fn finish_or_park(
        &self,
        server: AcpServer,
        session_id: String,
        prompt_id: Value,
        driven: DrivenPrompt,
    ) -> Result<RunOutput> {
        if let Some(permission) = driven.permission {
            let old = self.pending.lock().await.insert(
                session_id.clone(),
                PendingAcp {
                    server,
                    prompt_id,
                    permission,
                },
            );
            if let Some(old) = old {
                old.server.shutdown().await;
            }
        } else {
            server.shutdown().await;
        }
        Ok(RunOutput {
            events: driven.events,
            native_thread: Some(session_id),
        })
    }

    fn cwd(&self) -> String {
        self.identity
            .as_ref()
            .map(|identity| identity.home.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/tmp".to_string())
    }
}

#[async_trait]
impl Adapter for HermesAdapter {
    fn name(&self) -> &str {
        "hermes"
    }

    async fn probe(&self) -> Result<String> {
        let output = self.output(&["--version"]).await?;
        if !output.success {
            bail!("Hermes --version failed: {}", output.stderr.trim());
        }
        if !version_is_pinned(&output.stdout) {
            bail!(
                "Hermes CLI {:?} is not pinned (version {:?}, source {:?}); rebuild the image \
                 with the matching commit or extend the adapter",
                output.stdout.trim(),
                PINNED_CLI_VERSION,
                PINNED_SOURCE_COMMIT
            );
        }
        let check = self.output(&["acp", "--check"]).await?;
        if !check.success {
            let detail = if check.stderr.trim().is_empty() {
                check.stdout.trim()
            } else {
                check.stderr.trim()
            };
            bail!("Hermes ACP dependency check failed: {detail}");
        }
        Ok(format!(
            "{PINNED_CLI_VERSION} (source {PINNED_SOURCE_COMMIT}, ACP v{PINNED_ACP_PROTOCOL})"
        ))
    }

    async fn run(
        &self,
        thread_id: &str,
        native_thread: Option<&str>,
        input: &Value,
        events: EventSink,
    ) -> Result<RunOutput> {
        if let Some(session_id) = native_thread {
            validate_session_id(session_id)?;
            let pending = self.pending.lock().await.remove(session_id);
            if let Some(pending) = pending {
                return self
                    .resume_permission(session_id, input, pending, &events)
                    .await;
            }
        }
        self.drive_new_turn(thread_id, native_thread, input, &events)
            .await
    }
}

struct AcpServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl AcpServer {
    async fn send(&mut self, value: &Value) -> Result<()> {
        self.stdin
            .write_all((serde_json::to_string(value)? + "\n").as_bytes())
            .await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Value> {
        let line = read_line_capped(&mut self.stdout, LINE_CAP)
            .await?
            .ok_or_else(|| anyhow!("Hermes ACP closed the stream"))?;
        serde_json::from_str(&line).context("parse Hermes ACP JSON-RPC line")
    }

    async fn begin_request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = Value::from(self.next_id);
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        Ok(id)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.begin_request(method, params).await?;
        loop {
            let message = self.recv().await?;
            if message.get("id") == Some(&id) && message.get("method").is_none() {
                if let Some(error) = message.get("error") {
                    bail!("Hermes ACP {method} error: {error}");
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            // No client-side fs/terminal surface is advertised. Reject any
            // unexpected server request cleanly rather than leaving Hermes
            // blocked; notifications (including history replay) are ignored
            // while completing lifecycle requests.
            if message.get("method").is_some() && message.get("id").is_some() {
                self.reject_request(&message, -32601, "method not supported by hearth-guestd")
                    .await?;
            }
        }
    }

    async fn reject_request(&mut self, request: &Value, code: i64, message: &str) -> Result<()> {
        let Some(id) = request.get("id") else {
            return Ok(());
        };
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }))
        .await
    }

    async fn drive_prompt(
        &mut self,
        session_id: &str,
        prompt_id: &Value,
        events: &EventSink,
    ) -> Result<DrivenPrompt> {
        let mut translated = Translation::default();
        loop {
            let message = self.recv().await?;
            if message.get("id") == Some(prompt_id) && message.get("method").is_none() {
                translated.close_reasoning();
                translated.close_message();
                flush_events(&mut translated.events, events)?;
                if let Some(error) = message.get("error") {
                    translated.events.push(AdapterEvent::Failed {
                        message: format!("Hermes ACP prompt error: {error}"),
                    });
                } else {
                    let result = message.get("result").cloned().unwrap_or(Value::Null);
                    match result
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("end_turn")
                    {
                        "end_turn" => translated.events.push(AdapterEvent::Finished {
                            result: json!({
                                "summary": translated.text,
                                "hermes": result,
                            }),
                        }),
                        reason => translated.events.push(AdapterEvent::Failed {
                            message: format!("Hermes ACP stopped with {reason}"),
                        }),
                    }
                }
                return Ok(DrivenPrompt {
                    events: translated.events,
                    permission: None,
                });
            }

            let method = message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match method {
                "session/update" => {
                    let params = message.get("params").cloned().unwrap_or(Value::Null);
                    if params.get("sessionId").and_then(Value::as_str) == Some(session_id) {
                        translated.update(params.get("update").unwrap_or(&Value::Null));
                    } else {
                        translated.raw(message);
                    }
                }
                "session/request_permission" if message.get("id").is_some() => {
                    translated.close_reasoning();
                    translated.close_message();
                    flush_events(&mut translated.events, events)?;
                    let params = message.get("params").cloned().unwrap_or(Value::Null);
                    let options: HashSet<String> = params
                        .get("options")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|option| option.get("optionId").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect();
                    if options.is_empty() {
                        self.reject_request(&message, -32602, "permission request had no options")
                            .await?;
                        continue;
                    }
                    let request_id = message.get("id").cloned().unwrap_or(Value::Null);
                    let prompt = json!({
                        "kind": "permission",
                        "protocol": "acp",
                        "session_id": session_id,
                        "request_id": request_id,
                        "tool_call": params.get("toolCall").cloned().unwrap_or(Value::Null),
                        "options": params.get("options").cloned().unwrap_or_else(|| json!([])),
                    });
                    translated.events.push(AdapterEvent::AwaitingInput {
                        prompt: prompt.clone(),
                    });
                    return Ok(DrivenPrompt {
                        events: translated.events,
                        permission: Some(PermissionRequest {
                            request_id,
                            prompt,
                            options,
                        }),
                    });
                }
                _ if message.get("id").is_some() && !method.is_empty() => {
                    self.reject_request(&message, -32601, "method not supported by hearth-guestd")
                        .await?;
                    translated.raw(message);
                }
                _ => translated.raw(message),
            }
            flush_events(&mut translated.events, events)?;
        }
    }

    async fn shutdown(mut self) {
        let _ = self.stdin.shutdown().await;
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

struct PendingAcp {
    server: AcpServer,
    prompt_id: Value,
    permission: PermissionRequest,
}

struct PermissionRequest {
    request_id: Value,
    prompt: Value,
    options: HashSet<String>,
}

struct DrivenPrompt {
    events: Vec<AdapterEvent>,
    permission: Option<PermissionRequest>,
}

struct Translation {
    events: Vec<AdapterEvent>,
    turn_id: String,
    message_id: Option<String>,
    message_count: u64,
    reasoning_id: Option<String>,
    reasoning_count: u64,
    tool_count: u64,
    tool_ids: HashMap<String, String>,
    text: String,
}

impl Default for Translation {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            turn_id: new_ulid(),
            message_id: None,
            message_count: 0,
            reasoning_id: None,
            reasoning_count: 0,
            tool_count: 0,
            tool_ids: HashMap::new(),
            text: String::new(),
        }
    }
}

impl Translation {
    fn update(&mut self, update: &Value) {
        match update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "agent_thought_chunk" => {
                let text = update
                    .get("content")
                    .and_then(|content| content.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.is_empty() {
                    return;
                }
                // AG-UI transcript items are chronological segments. Thought
                // content starts a new segment rather than remaining nested
                // inside assistant text that began before a tool/reasoning
                // transition.
                self.close_message();
                let message_id = match &self.reasoning_id {
                    Some(message_id) => message_id.clone(),
                    None => {
                        let message_id =
                            format!("{REASONING_ID}-{}-{}", self.turn_id, self.reasoning_count);
                        self.reasoning_count += 1;
                        self.events
                            .push(AdapterEvent::Event(AgentEvent::ReasoningStart {
                                message_id: message_id.clone(),
                            }));
                        self.events
                            .push(AdapterEvent::Event(AgentEvent::ReasoningMessageStart {
                                message_id: message_id.clone(),
                                role: "reasoning".to_string(),
                            }));
                        self.reasoning_id = Some(message_id.clone());
                        message_id
                    }
                };
                self.events
                    .push(AdapterEvent::Event(AgentEvent::ReasoningMessageContent {
                        message_id,
                        delta: text.to_string(),
                    }));
            }
            "agent_message_chunk" => {
                self.close_reasoning();
                let text = update
                    .get("content")
                    .and_then(|content| content.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.is_empty() {
                    return;
                }
                if self.message_id.is_none() {
                    let message_id =
                        format!("hermes-message-{}-{}", self.turn_id, self.message_count);
                    self.message_count += 1;
                    self.events
                        .push(AdapterEvent::Event(AgentEvent::TextMessageStart {
                            message_id: message_id.clone(),
                            role: "assistant".to_string(),
                        }));
                    self.message_id = Some(message_id);
                }
                self.text.push_str(text);
                self.events
                    .push(AdapterEvent::Event(AgentEvent::TextMessageContent {
                        message_id: self.message_id.clone().expect("message just opened"),
                        delta: text.to_string(),
                    }));
            }
            "tool_call" => {
                self.close_reasoning();
                self.close_message();
                let Some(provider_id) = update.get("toolCallId").and_then(Value::as_str) else {
                    self.raw(update.clone());
                    return;
                };
                let id = format!("hermes-tool-{}-{}", self.turn_id, self.tool_count);
                self.tool_count += 1;
                self.tool_ids.insert(provider_id.to_string(), id.clone());
                let title = update
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("hermes tool")
                    .to_string();
                self.events
                    .push(AdapterEvent::Event(AgentEvent::ToolCallStart {
                        tool_call_id: id.clone(),
                        tool_call_name: title,
                        parent_message_id: None,
                    }));
                if let Some(args) = update.get("rawInput") {
                    self.events
                        .push(AdapterEvent::Event(AgentEvent::ToolCallArgs {
                            tool_call_id: id,
                            delta: args.to_string(),
                        }));
                }
            }
            "tool_call_update" => {
                let Some(provider_id) = update.get("toolCallId").and_then(Value::as_str) else {
                    self.raw(update.clone());
                    return;
                };
                let status = update.get("status").and_then(Value::as_str);
                if matches!(status, Some("completed" | "failed")) {
                    let Some(id) = self.tool_ids.remove(provider_id) else {
                        self.raw(update.clone());
                        return;
                    };
                    self.events
                        .push(AdapterEvent::Event(AgentEvent::ToolCallEnd {
                            tool_call_id: id.clone(),
                        }));
                    let content = tool_result_text(update);
                    if !content.is_empty() {
                        self.events
                            .push(AdapterEvent::Event(AgentEvent::ToolCallResult {
                                message_id: format!("{id}-result"),
                                tool_call_id: id.clone(),
                                content,
                                role: Some("tool".to_string()),
                            }));
                    }
                } else {
                    self.raw(update.clone());
                }
            }
            // AG-UI reasoning events cover thought chunks. Preserve other ACP
            // plan/usage/session-info updates losslessly as RAW.
            _ => self.raw(update.clone()),
        }
    }

    fn close_reasoning(&mut self) {
        if let Some(message_id) = self.reasoning_id.take() {
            self.events
                .push(AdapterEvent::Event(AgentEvent::ReasoningMessageEnd {
                    message_id: message_id.clone(),
                }));
            self.events
                .push(AdapterEvent::Event(AgentEvent::ReasoningEnd { message_id }));
        }
    }

    fn close_message(&mut self) {
        if let Some(message_id) = self.message_id.take() {
            self.events
                .push(AdapterEvent::Event(AgentEvent::TextMessageEnd {
                    message_id,
                }));
        }
    }

    fn raw(&mut self, event: Value) {
        self.events.push(AdapterEvent::Event(AgentEvent::Raw {
            event,
            source: Some("hermes-acp".to_string()),
        }));
    }
}

fn tool_result_text(update: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(content) = update.get("content").and_then(Value::as_array) {
        for item in content {
            if let Some(text) = item
                .get("content")
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
            {
                parts.push(text.to_string());
            }
        }
    }
    if parts.is_empty() {
        if let Some(raw) = update.get("rawOutput") {
            parts.push(
                raw.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| raw.to_string()),
            );
        }
    }
    parts.join("\n")
}

fn input_text(input: &Value) -> Result<&str> {
    input
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| input.as_str())
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| anyhow!("Hermes ACP run requires non-empty text input"))
}

fn version_is_pinned(banner: &str) -> bool {
    banner.contains(&format!("Hermes Agent v{PINNED_CLI_VERSION}"))
        && (banner.contains(&format!("upstream {PINNED_SOURCE_COMMIT}"))
            || banner.contains(&format!("local {PINNED_SOURCE_COMMIT}")))
}

fn permission_decision(input: &Value, options: &HashSet<String>) -> Result<String> {
    let raw = input_text(input)?.trim().to_ascii_lowercase();
    let normalized = raw.replace([' ', '-'], "_");
    let candidates: &[&str] = match normalized.as_str() {
        "yes" | "y" | "approve" | "approved" | "allow" | "once" | "allow_once" => &["allow_once"],
        "session" | "allow_session" => &["allow_session"],
        "always" | "allow_always" => &["allow_always"],
        "no" | "n" | "deny" | "reject" => &["deny", "deny_always"],
        exact => &[exact],
    };
    candidates
        .iter()
        .find(|candidate| options.contains(**candidate))
        .map(|candidate| (*candidate).to_string())
        .ok_or_else(|| anyhow!("approval decision {:?} is not one of {:?}", raw, options))
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        bail!("Hermes ACP returned an invalid session id");
    }
    Ok(())
}

struct CappedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(reader: R) -> Result<CappedBytes> {
    let mut bytes = Vec::new();
    reader.take(OUTPUT_CAP + 1).read_to_end(&mut bytes).await?;
    let truncated = bytes.len() as u64 > OUTPUT_CAP;
    if truncated {
        bytes.truncate(OUTPUT_CAP as usize);
    }
    Ok(CappedBytes { bytes, truncated })
}

struct ProcessOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_real_acp_message_chunks() {
        let mut translated = Translation::default();
        translated.update(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "HER" },
        }));
        translated.update(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "MES" },
        }));
        translated.close_message();
        assert_eq!(translated.text, "HERMES");
        assert!(matches!(
            translated.events.first(),
            Some(AdapterEvent::Event(AgentEvent::TextMessageStart { .. }))
        ));
        assert!(matches!(
            translated.events.last(),
            Some(AdapterEvent::Event(AgentEvent::TextMessageEnd { .. }))
        ));
    }

    #[test]
    fn translates_acp_tool_start_and_completion() {
        let mut translated = Translation::default();
        translated.update(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-1",
            "title": "terminal: echo hi",
            "rawInput": { "command": "echo hi" },
        }));
        translated.update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tc-1",
            "status": "completed",
            "content": [{
                "type": "content",
                "content": { "type": "text", "text": "hi" }
            }],
        }));
        assert!(translated.events.iter().any(|event| matches!(
            event,
            AdapterEvent::Event(AgentEvent::ToolCallResult { content, .. }) if content == "hi"
        )));
    }

    #[test]
    fn assistant_text_is_segmented_around_tools_in_wire_order() {
        let mut translated = Translation::default();
        translated.update(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "Let me look." },
        }));
        translated.update(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-1",
            "title": "web search",
            "rawInput": { "query": "weather" },
        }));
        translated.update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tc-1",
            "status": "completed",
            "content": [{
                "type": "content",
                "content": { "type": "text", "text": "sunny" }
            }],
        }));
        translated.update(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "It is sunny." },
        }));
        translated.close_message();

        let types: Vec<&str> = translated
            .events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::Event(AgentEvent::TextMessageStart { .. }) => Some("text-start"),
                AdapterEvent::Event(AgentEvent::TextMessageContent { .. }) => Some("text"),
                AdapterEvent::Event(AgentEvent::TextMessageEnd { .. }) => Some("text-end"),
                AdapterEvent::Event(AgentEvent::ToolCallStart { .. }) => Some("tool-start"),
                AdapterEvent::Event(AgentEvent::ToolCallArgs { .. }) => Some("tool-args"),
                AdapterEvent::Event(AgentEvent::ToolCallEnd { .. }) => Some("tool-end"),
                AdapterEvent::Event(AgentEvent::ToolCallResult { .. }) => Some("tool-result"),
                _ => None,
            })
            .collect();
        assert_eq!(
            types,
            vec![
                "text-start",
                "text",
                "text-end",
                "tool-start",
                "tool-args",
                "tool-end",
                "tool-result",
                "text-start",
                "text",
                "text-end",
            ]
        );

        let message_ids: Vec<&str> = translated
            .events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::Event(AgentEvent::TextMessageStart { message_id, .. }) => {
                    Some(message_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(message_ids.len(), 2);
        assert_ne!(message_ids[0], message_ids[1]);
    }

    #[test]
    fn translates_acp_thought_chunks_to_agui_reasoning() {
        let mut translated = Translation::default();
        translated.update(&json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "CON" },
        }));
        translated.update(&json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "SIDER" },
        }));
        translated.close_reasoning();

        assert!(matches!(
            translated.events.first(),
            Some(AdapterEvent::Event(AgentEvent::ReasoningStart { .. }))
        ));
        assert_eq!(
            translated
                .events
                .iter()
                .filter_map(|event| match event {
                    AdapterEvent::Event(AgentEvent::ReasoningMessageContent { delta, .. }) =>
                        Some(delta.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            "CONSIDER"
        );
        assert!(matches!(
            translated.events.last(),
            Some(AdapterEvent::Event(AgentEvent::ReasoningEnd { .. }))
        ));
    }

    #[test]
    fn approval_decisions_must_match_offered_acp_options() {
        let options = HashSet::from([
            "allow_once".to_string(),
            "allow_session".to_string(),
            "deny".to_string(),
        ]);
        assert_eq!(
            permission_decision(&json!({ "text": "allow once" }), &options).unwrap(),
            "allow_once"
        );
        assert!(permission_decision(&json!({ "text": "always" }), &options).is_err());
    }

    #[test]
    fn validates_only_the_external_acp_session_boundary() {
        assert!(validate_session_id("fe9e3089-ccac-4609-b717-47f82bf41f81").is_ok());
        assert!(validate_session_id("--config=/tmp/other").is_err());
    }

    #[test]
    fn accepts_source_pin_in_both_real_version_banner_forms() {
        assert!(version_is_pinned(
            "Hermes Agent v0.18.2 (2026.7.7.2) · upstream 2ea39dae"
        ));
        assert!(version_is_pinned(
            "Hermes Agent v0.18.2 (2026.7.7.2) · upstream 4a69a662 · local 2ea39dae (+15667 carried commits)"
        ));
        assert!(!version_is_pinned(
            "Hermes Agent v0.18.2 (2026.7.7.2) · upstream 4a69a662"
        ));
    }
}
