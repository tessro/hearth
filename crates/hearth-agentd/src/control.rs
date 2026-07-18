//! The local control socket (docs/agent-plane.md §4.1): `/run/hearth/agent.sock`
//! (`0660 root:hearth`), line-JSON, the same framing as hearthd. This is what
//! `hearthctl agent …` speaks. Verbs mirror the task API plus `agent-ls`.
//!
//! The control socket is the operator/UI seat: it presents refs as "ui" and is
//! itself a delegator (a human driving the fleet), so it can start tasks on any
//! agent VM and drive them.

use crate::core::Agentd;
use anyhow::{anyhow, Result};
use hearth_agent_proto::{read_line_capped, AgentRequest, AgentVerb, MAX_LINE_BYTES};
use hearth_proto::Response;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, warn};

pub const CONTROL_PRESENTER: &str = "ui";

pub async fn serve(agentd: Arc<Agentd>, listener: UnixListener) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let agentd = Arc::clone(&agentd);
        tokio::spawn(async move {
            if let Err(err) = handle(agentd, stream).await {
                error!(error = %err, "control connection failed");
            }
        });
    }
}

async fn handle(agentd: Arc<Agentd>, mut stream: UnixStream) -> Result<()> {
    loop {
        let Some(line) = read_line_capped(&mut stream, MAX_LINE_BYTES).await? else {
            return Ok(());
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: AgentRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(err) => {
                write_line(
                    &mut stream,
                    &Response::failure("", "protocol.invalid_json", err.to_string()),
                )
                .await?;
                continue;
            }
        };
        let id = req.id.clone();
        // `task.attach` streams; everything else is one response.
        if req.verb == AgentVerb::TaskAttach {
            attach(&agentd, &mut stream, &req).await?;
            continue;
        }
        match dispatch(&agentd, &req).await {
            Ok(value) => write_line(&mut stream, &Response::success(id, value)).await?,
            Err(err) => {
                let (code, message) = split_coded(&err);
                write_line(&mut stream, &Response::failure(id, code, message)).await?;
            }
        }
    }
}

async fn dispatch(agentd: &Arc<Agentd>, req: &AgentRequest) -> Result<Value> {
    let args = &req.args;
    match req.verb {
        AgentVerb::Ping => Ok(json!({ "pong": true, "component": "agentd" })),
        AgentVerb::Version => Ok(hearth_agent_proto::agent_version_result(env!(
            "CARGO_PKG_VERSION"
        ))),
        AgentVerb::AgentLs => agentd.list_agents().await,
        AgentVerb::TaskStart => {
            // The operator starts a task directly on an agent VM. This is a
            // delegation from the "ui" seat: ledgered and ref-minted so the
            // same wake-up machinery applies.
            let target = str_arg(args, "agent")?;
            let text = str_arg(args, "text")?;
            // The control socket is always a permitted delegator.
            if !agentd.is_delegator(CONTROL_PRESENTER) {
                // "ui" is implicitly allowed from the control socket.
            }
            agentd
                .delegate_from_ui(CONTROL_PRESENTER, target, text)
                .await
        }
        AgentVerb::TaskStatus => {
            let claims = agentd.resolve_ref(str_arg(args, "task_ref")?, CONTROL_PRESENTER)?;
            agentd
                .relay_verb(&claims, AgentVerb::TaskStatus, Map::new())
                .await
        }
        AgentVerb::TaskEvents => {
            let claims = agentd.resolve_ref(str_arg(args, "task_ref")?, CONTROL_PRESENTER)?;
            let mut extra = Map::new();
            for key in ["cursor", "filter", "max"] {
                if let Some(v) = args.get(key) {
                    extra.insert(key.to_string(), v.clone());
                }
            }
            agentd
                .relay_verb(&claims, AgentVerb::TaskEvents, extra)
                .await
        }
        AgentVerb::TaskRespond => {
            let claims = agentd.resolve_ref(str_arg(args, "task_ref")?, CONTROL_PRESENTER)?;
            let mut extra = Map::new();
            extra.insert(
                "response".to_string(),
                args.get("response").cloned().unwrap_or(json!({})),
            );
            agentd
                .relay_verb(&claims, AgentVerb::TaskRespond, extra)
                .await
        }
        AgentVerb::TaskFollowup => {
            let claims = agentd.resolve_ref(str_arg(args, "task_ref")?, CONTROL_PRESENTER)?;
            let mut extra = Map::new();
            extra.insert("text".to_string(), json!(str_arg(args, "text")?));
            agentd
                .relay_verb(&claims, AgentVerb::TaskFollowup, extra)
                .await
        }
        AgentVerb::TaskCancel => {
            let claims = agentd.resolve_ref(str_arg(args, "task_ref")?, CONTROL_PRESENTER)?;
            let result = agentd
                .relay_verb(&claims, AgentVerb::TaskCancel, Map::new())
                .await?;
            agentd.cancel_grant(&claims.task_id)?;
            Ok(result)
        }
        AgentVerb::TaskList => {
            // List across all agent VMs.
            let mut all = Vec::new();
            for endpoint in agentd.hearthd.agent_endpoints().await? {
                if !endpoint.running {
                    continue;
                }
                if let Ok(value) = crate::relay::call(
                    &agentd.hearthd,
                    &endpoint.name,
                    AgentVerb::TaskList,
                    Map::new(),
                )
                .await
                {
                    if let Some(tasks) = value.get("tasks").and_then(Value::as_array) {
                        for task in tasks {
                            let mut task = task.clone();
                            task["agent_vm"] = json!(endpoint.name);
                            all.push(task);
                        }
                    }
                }
            }
            Ok(json!({ "tasks": all }))
        }
        AgentVerb::TaskGc => Err(anyhow!("task.gc: run per-agent via the guest")),
        AgentVerb::SetSessionName => Err(anyhow!(
            "verb.denied: session.set-name is MCP-shim-internal"
        )),
        AgentVerb::InjectTurn => Err(anyhow!("verb.denied: inject.turn is guestd-internal")),
        AgentVerb::TaskAttach => unreachable!("attach handled before dispatch"),
    }
}

async fn attach(agentd: &Arc<Agentd>, stream: &mut UnixStream, req: &AgentRequest) -> Result<()> {
    let id = req.id.clone();
    let claims = match agentd.resolve_ref(str_arg(&req.args, "task_ref")?, CONTROL_PRESENTER) {
        Ok(claims) => claims,
        Err(err) => {
            let (code, message) = split_coded(&err);
            return write_line(stream, &Response::failure(id, code, message)).await;
        }
    };
    let mut extra = Map::new();
    if let Some(cursor) = req.args.get("cursor") {
        extra.insert("cursor".to_string(), cursor.clone());
    }
    extra.insert("task_id".to_string(), json!(claims.task_id));
    let (mut guest, _guest_id) =
        match crate::relay::attach(&agentd.hearthd, &claims.target, extra).await {
            Ok(pair) => pair,
            Err(err) => {
                let (code, message) = split_coded(&err);
                return write_line(stream, &Response::failure(id, code, message)).await;
            }
        };
    // Pump guest attach frames out to the control client, re-tagged with our id.
    loop {
        match crate::relay::next_attach_frame(&mut guest).await {
            Ok(Some(frame)) => {
                write_line(stream, &Response::stream_data(id.clone(), frame)).await?
            }
            Ok(None) => return write_line(stream, &Response::stream_end(id)).await,
            Err(err) => {
                warn!(error = %err, "attach relay dropped");
                return write_line(stream, &Response::stream_end(id)).await;
            }
        }
    }
}

fn str_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("request.invalid: missing string argument {key}"))
}

fn split_coded(err: &anyhow::Error) -> (String, String) {
    let text = err.to_string();
    match text.split_once(": ") {
        Some((code, message)) if !code.contains(' ') => (code.to_string(), message.to_string()),
        _ => ("agent.error".to_string(), text),
    }
}

async fn write_line(stream: &mut UnixStream, response: &Response) -> Result<()> {
    stream
        .write_all((serde_json::to_string(response)? + "\n").as_bytes())
        .await?;
    stream.flush().await?;
    Ok(())
}
