//! agentd's MCP server (docs/agent-plane.md §4.3, §7.1). Speaks stdio JSON-RPC
//! framing directly over brokered vsock connections from guest shims — no HTTP
//! (§13.7). The calling agent occupies the "user" seat of the callee's task;
//! the presenting VM (identity = socket path) is the delegation initiator.
//!
//! Streaming to an LLM caller is cursor-based polling (D9): `wait_for` and
//! `task_events` let the model spend context deliberately. MCP progress
//! notifications are display garnish only (§7.2).

use crate::core::Agentd;
use anyhow::{anyhow, Context, Result};
use hearth_agent_proto::{
    read_line_capped, AgentVerb, MAX_LINE_BYTES, MAX_SESSION_NAME_CHARS, MCP_TOOLS,
};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub struct McpServer {
    agentd: Arc<Agentd>,
}

impl McpServer {
    pub fn new(agentd: Arc<Agentd>) -> Arc<Self> {
        Arc::new(Self { agentd })
    }

    /// Serve one shim connection. `vm` is the presenting VM (from the brokered
    /// listener); `thread_id` is the shim's session (advisory here).
    pub async fn serve<S>(&self, vm: &str, thread_id: &str, mut stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        loop {
            let Some(line) = read_line_capped(&mut stream, MAX_LINE_BYTES).await? else {
                return Ok(());
            };
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(msg) => msg,
                Err(err) => {
                    warn_line(&mut stream, &format!("parse error: {err}")).await?;
                    continue;
                }
            };
            let id = msg.get("id").cloned();
            let method = msg
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // Notifications (no id) get no response.
            if id.is_none() {
                continue;
            }
            let response = match self.handle(vm, thread_id, method, &msg).await {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(err) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32000, "message": err.to_string() },
                }),
            };
            stream
                .write_all((serde_json::to_string(&response)? + "\n").as_bytes())
                .await?;
            stream.flush().await?;
        }
    }

    async fn handle(&self, vm: &str, _thread_id: &str, method: &str, msg: &Value) -> Result<Value> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2025-06-18",
                "serverInfo": { "name": "hearth-agentd", "version": hearth_proto::VERSION },
                "capabilities": { "tools": {} },
            })),
            "tools/list" => Ok(json!({ "tools": tool_schemas() })),
            "tools/call" => {
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args = params
                    .get("arguments")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let result = self.call_tool(vm, _thread_id, name, &args).await?;
                Ok(json!({
                    "content": [{ "type": "text", "text": serde_json::to_string(&result)? }],
                    "isError": false,
                }))
            }
            other => Err(anyhow!("unsupported MCP method {other}")),
        }
    }

    async fn call_tool(
        &self,
        vm: &str,
        shim_thread: &str,
        name: &str,
        args: &Map<String, Value>,
    ) -> Result<Value> {
        match name {
            "set_hostname" => {
                let hostname = str_arg(args, "hostname")?;
                self.agentd.hearthd.set_hostname(vm, hostname).await
            }
            "set_session_name" => {
                if shim_thread.is_empty() {
                    return Err(anyhow!(
                        "session.unbound: the MCP shim did not identify a session thread"
                    ));
                }
                let requested_name = str_arg(args, "name")?;
                let mut extra = Map::new();
                extra.insert("thread_id".to_string(), json!(shim_thread));
                extra.insert("name".to_string(), json!(requested_name));
                let renamed =
                    crate::relay::call(&self.agentd.hearthd, vm, AgentVerb::SetSessionName, extra)
                        .await?;
                let stored_name = renamed
                    .get("session_name")
                    .and_then(Value::as_str)
                    .unwrap_or(requested_name);
                Ok(json!({ "name": stored_name }))
            }
            "list_agents" => self.agentd.list_agents().await,
            "delegate" => {
                let target = str_arg(args, "agent")?;
                let text = str_arg(args, "task")?;
                let wait_seconds = args
                    .get("wait_seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                // The initiator thread — where the completion wake-up is
                // injected — is the *calling agent's own session*, which the
                // shim carried in its MCP hello (§2.4). Without this, every
                // delegation would record `initiator_thread: None` and its
                // wake-up would be dropped by the ledger's no-thread branch.
                // An explicit arg overrides only for advanced callers/tests.
                let initiator_thread = args
                    .get("initiator_thread")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or(if shim_thread.is_empty() {
                        None
                    } else {
                        Some(shim_thread)
                    });
                let result = self
                    .agentd
                    .delegate(vm, initiator_thread, target, text)
                    .await?;
                if wait_seconds > 0 {
                    if let Some(task_ref) = result.get("task_ref").and_then(Value::as_str) {
                        let waited = self
                            .wait_for(vm, task_ref, wait_seconds, None)
                            .await
                            .unwrap_or(Value::Null);
                        return Ok(json!({ "delegated": result, "waited": waited }));
                    }
                }
                Ok(result)
            }
            "wait_for" => {
                let task_ref = str_arg(args, "task_ref")?;
                let timeout = args
                    .get("timeout_seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(30);
                let cursor = args.get("cursor").and_then(Value::as_str);
                self.wait_for(vm, task_ref, timeout, cursor).await
            }
            "task_events" => {
                let task_ref = str_arg(args, "task_ref")?;
                let claims = self.agentd.resolve_ref(task_ref, vm)?;
                let mut extra = Map::new();
                if let Some(cursor) = args.get("cursor") {
                    extra.insert("cursor".to_string(), cursor.clone());
                }
                if let Some(filter) = args.get("filter") {
                    extra.insert("filter".to_string(), filter.clone());
                }
                if let Some(max) = args.get("max_events") {
                    extra.insert("max".to_string(), max.clone());
                }
                self.agentd
                    .relay_verb(&claims, AgentVerb::TaskEvents, extra)
                    .await
            }
            "task_respond" => {
                let task_ref = str_arg(args, "task_ref")?;
                let response = args
                    .get("response")
                    .cloned()
                    .ok_or_else(|| anyhow!("task_respond needs a response"))?;
                let claims = self.agentd.resolve_ref(task_ref, vm)?;
                let mut extra = Map::new();
                extra.insert("response".to_string(), response);
                self.agentd
                    .relay_verb(&claims, AgentVerb::TaskRespond, extra)
                    .await
            }
            "task_status" => {
                let task_ref = str_arg(args, "task_ref")?;
                let claims = self.agentd.resolve_ref(task_ref, vm)?;
                self.agentd
                    .relay_verb(&claims, AgentVerb::TaskStatus, Map::new())
                    .await
            }
            "task_cancel" => {
                let task_ref = str_arg(args, "task_ref")?;
                let claims = self.agentd.resolve_ref(task_ref, vm)?;
                let result = self
                    .agentd
                    .relay_verb(&claims, AgentVerb::TaskCancel, Map::new())
                    .await?;
                self.agentd.cancel_grant(&claims.task_id)?;
                Ok(result)
            }
            other => Err(anyhow!("unknown tool {other}")),
        }
    }

    /// Long-poll a delegated task until its state changes, it reaches a
    /// terminal/awaiting state, or the timeout elapses. Returns state, the new
    /// events since `cursor`, and the next cursor (the streaming workhorse,
    /// §7.1).
    async fn wait_for(
        &self,
        vm: &str,
        task_ref: &str,
        timeout_seconds: u64,
        cursor: Option<&str>,
    ) -> Result<Value> {
        let claims = self.agentd.resolve_ref(task_ref, vm)?;
        let deadline = self.agentd.now() + timeout_seconds as i64;
        let mut cursor = cursor.map(str::to_string);
        loop {
            let status = self
                .agentd
                .relay_verb(&claims, AgentVerb::TaskStatus, Map::new())
                .await?;
            let state = status.get("state").and_then(Value::as_str).unwrap_or("");
            let mut events_extra = Map::new();
            if let Some(cursor) = &cursor {
                events_extra.insert("cursor".to_string(), json!(cursor));
            }
            let events = self
                .agentd
                .relay_verb(&claims, AgentVerb::TaskEvents, events_extra)
                .await?;
            if let Some(next) = events.get("cursor").and_then(Value::as_str) {
                cursor = Some(next.to_string());
            }
            let settled = matches!(
                state,
                "completed" | "failed" | "canceled" | "awaiting_input"
            );
            let has_events = events
                .get("events")
                .and_then(Value::as_array)
                .map(|e| !e.is_empty())
                .unwrap_or(false);
            if settled || has_events || self.agentd.now() >= deadline {
                return Ok(json!({
                    "state": state,
                    "events": events.get("events").cloned().unwrap_or(json!([])),
                    "cursor": cursor,
                    "status": status,
                }));
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}

fn tool_schemas() -> Vec<Value> {
    MCP_TOOLS
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "description": tool_description(name),
                "inputSchema": tool_input_schema(name),
            })
        })
        .collect()
}

fn tool_description(name: &str) -> &'static str {
    match name {
        "set_hostname" => "Change this VM's service-discovery hostname. The fixed VM id and active tasks do not change.",
        "set_session_name" => "Replace this session's display name with a short, descriptive name. Call at the beginning of every session, and when there is a substantive change in the session's purpose.",
        "list_agents" => "List agent-enabled VMs, their adapters, and task counts.",
        "delegate" => {
            "Delegate a task to another agent; optionally wait_seconds for a first result."
        }
        "wait_for" => "Long-poll a delegated task until it changes, needs input, or ends.",
        "task_events" => "Read a delegated task's event log from a cursor, with a filter.",
        "task_respond" => "Answer a delegated task that is awaiting input (starts a new run).",
        "task_status" => "Get a delegated task's current status.",
        "task_cancel" => "Cancel a delegated task (also revokes it in the ledger).",
        _ => "",
    }
}

fn tool_input_schema(name: &str) -> Value {
    let ref_prop = json!({ "task_ref": { "type": "string" } });
    match name {
        "set_hostname" => json!({
            "type": "object",
            "properties": {
                "hostname": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 63,
                    "pattern": "^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$"
                }
            },
            "required": ["hostname"],
            "additionalProperties": false,
        }),
        "set_session_name" => json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_SESSION_NAME_CHARS,
                    "description": "The complete new display name for this session."
                },
            },
            "required": ["name"],
            "additionalProperties": false,
        }),
        "list_agents" => json!({ "type": "object", "properties": {} }),
        "delegate" => json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string" },
                "task": { "type": "string" },
                "wait_seconds": { "type": "integer" },
            },
            "required": ["agent", "task"],
        }),
        "wait_for" => json!({
            "type": "object",
            "properties": {
                "task_ref": { "type": "string" },
                "timeout_seconds": { "type": "integer" },
                "cursor": { "type": "string" },
            },
            "required": ["task_ref"],
        }),
        "task_events" => json!({
            "type": "object",
            "properties": {
                "task_ref": { "type": "string" },
                "cursor": { "type": "string" },
                "filter": { "type": "string" },
                "max_events": { "type": "integer" },
            },
            "required": ["task_ref"],
        }),
        "task_respond" => json!({
            "type": "object",
            "properties": {
                "task_ref": { "type": "string" },
                "response": {},
            },
            "required": ["task_ref", "response"],
        }),
        _ => json!({ "type": "object", "properties": ref_prop, "required": ["task_ref"] }),
    }
}

fn str_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string argument {key}"))
}

async fn warn_line<S: AsyncWrite + Unpin>(stream: &mut S, message: &str) -> Result<()> {
    let err =
        json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": message } });
    stream
        .write_all((serde_json::to_string(&err)? + "\n").as_bytes())
        .await?;
    stream.flush().await.context("flush mcp error")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_hostname_tool_has_a_dns_label_schema() {
        let tools = tool_schemas();
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == "set_hostname")
            .unwrap();
        assert_eq!(
            tool["inputSchema"]["properties"]["hostname"]["pattern"],
            json!("^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$")
        );
        assert_eq!(tool["inputSchema"]["required"], json!(["hostname"]));
    }
}
