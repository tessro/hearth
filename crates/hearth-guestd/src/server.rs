//! The task-verb server (docs/agent-plane.md §3.5), reached host→guest on
//! port 1027. Same `Request`/`Response` line-JSON framing as the machine
//! plane, but with the agent verb set. `task.attach` streams like
//! `logs --follow`: replay from the cursor, then follow.

use crate::engine::Engine;
use anyhow::{anyhow, Result};
use hearth_agent_proto::{
    read_line_capped, AgentRequest, AgentVerb, Hello, AGENT_PROTOCOL_VERSION, MAX_LINE_BYTES,
};
use hearth_proto::Response;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub async fn serve_connection<S>(engine: Arc<Engine>, mut stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // The first application frame on every task-verb channel is the mandatory
    // version hello (§5.3). Refuse skew before interpreting any task content.
    loop {
        let Some(line) = read_line_capped(&mut stream, MAX_LINE_BYTES).await? else {
            return Ok(());
        };
        if line.trim().is_empty() {
            continue;
        }
        let hello = match serde_json::from_str::<Hello>(&line) {
            Ok(hello) => hello,
            Err(err) => {
                write_line(
                    &mut stream,
                    &Response::failure(
                        "hello",
                        "protocol.hello_required",
                        format!("first frame must be an agent-plane hello: {err}"),
                    ),
                )
                .await?;
                return Ok(());
            }
        };
        if hello.component != "agentd" && hello.component != "hearthctl-agent" {
            write_line(
                &mut stream,
                &Response::failure(
                    "hello",
                    "protocol.invalid_component",
                    format!("component {:?} may not drive guestd", hello.component),
                ),
            )
            .await?;
            return Ok(());
        }
        if hello.proto != AGENT_PROTOCOL_VERSION {
            write_line(
                &mut stream,
                &Response::failure(
                    "hello",
                    "protocol.version_mismatch",
                    format!(
                        "guestd protocol {} does not support peer protocol {}",
                        AGENT_PROTOCOL_VERSION, hello.proto
                    ),
                ),
            )
            .await?;
            return Ok(());
        }
        write_line(
            &mut stream,
            &Response::success("hello", json!({ "proto": AGENT_PROTOCOL_VERSION })),
        )
        .await?;
        break;
    }

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
        if req.verb == AgentVerb::TaskAttach {
            attach(&engine, &mut stream, &req).await?;
            continue;
        }
        let id = req.id.clone();
        match dispatch(&engine, req).await {
            Ok(value) => write_line(&mut stream, &Response::success(id, value)).await?,
            Err(err) => {
                let (code, message) = split_coded(&err);
                write_line(&mut stream, &Response::failure(id, code, message)).await?
            }
        }
    }
}

async fn dispatch(engine: &Arc<Engine>, req: AgentRequest) -> Result<Value> {
    let args = &req.args;
    match req.verb {
        AgentVerb::Ping => Ok(json!({ "pong": true, "component": "guestd" })),
        AgentVerb::Version => Ok(hearth_agent_proto::agent_version_result(env!(
            "CARGO_PKG_VERSION"
        ))),
        AgentVerb::AgentLs => Ok(json!({ "agents": engine.adapters() })),
        AgentVerb::TaskStart => {
            let agent = str_arg(args, "agent")?;
            let text = str_arg(args, "text")?;
            let detach = args.get("detach").and_then(Value::as_bool).unwrap_or(true);
            let initiator = args
                .get("initiator")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            // agentd may pin the task_id so it can ledger the grant first (§7.1).
            let task_id = args
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let summary = engine
                .start(agent, text, initiator, detach, task_id)
                .await?;
            Ok(serde_json::to_value(summary)?)
        }
        AgentVerb::TaskStatus => Ok(serde_json::to_value(
            engine.status(str_arg(args, "task_id")?)?,
        )?),
        AgentVerb::TaskEvents => {
            let task_id = str_arg(args, "task_id")?;
            let cursor = args.get("cursor").and_then(Value::as_str);
            let max = args.get("max").and_then(Value::as_u64).unwrap_or(256) as usize;
            let (events, next) = engine.events(task_id, cursor, max).await?;
            let events = maybe_filter(events, args.get("filter").and_then(Value::as_str));
            Ok(json!({ "events": events, "cursor": next }))
        }
        AgentVerb::TaskRespond => {
            let task_id = str_arg(args, "task_id")?;
            let response = args
                .get("response")
                .cloned()
                .ok_or_else(|| anyhow!("request.invalid: missing response"))?;
            Ok(serde_json::to_value(engine.respond(task_id, response)?)?)
        }
        AgentVerb::TaskFollowup => Ok(serde_json::to_value(
            engine.follow_up(str_arg(args, "task_id")?, str_arg(args, "text")?)?,
        )?),
        AgentVerb::TaskCancel => Ok(serde_json::to_value(
            engine.cancel(str_arg(args, "task_id")?)?,
        )?),
        AgentVerb::TaskList => Ok(json!({ "tasks": engine.list() })),
        AgentVerb::TaskGc => {
            let keep = args.get("keep").and_then(Value::as_u64).unwrap_or(20) as usize;
            Ok(json!({ "removed": engine.gc(keep)? }))
        }
        AgentVerb::SetSessionName => Ok(serde_json::to_value(
            engine
                .set_session_name(str_arg(args, "thread_id")?, str_arg(args, "name")?)
                .await?,
        )?),
        AgentVerb::InjectTurn => {
            let delivery_id = str_arg(args, "delivery_id")?;
            let thread_id = str_arg(args, "thread_id")?;
            let text = str_arg(args, "text")?;
            let injected = engine.inject_turn(delivery_id, thread_id, text)?;
            // Idempotent ack: a duplicate is acknowledged without re-injecting.
            Ok(json!({ "injected": injected, "delivery_id": delivery_id }))
        }
        AgentVerb::TaskAttach => unreachable!("attach handled before dispatch"),
    }
}

/// `task.attach`: replay events from the cursor as stream frames, then follow
/// until the task is terminal or the client disconnects (§3.5).
async fn attach<S>(engine: &Arc<Engine>, stream: &mut S, req: &AgentRequest) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let id = req.id.clone();
    let task_id = match str_arg(&req.args, "task_id") {
        Ok(task_id) => task_id.to_string(),
        Err(err) => {
            let (code, message) = split_coded(&err);
            return write_line(stream, &Response::failure(id, code, message)).await;
        }
    };
    let mut cursor = req
        .args
        .get("cursor")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut updates = match engine.subscribe(&task_id) {
        Ok(rx) => rx,
        Err(err) => {
            let (code, message) = split_coded(&err);
            return write_line(stream, &Response::failure(id, code, message)).await;
        }
    };
    loop {
        let (events, next) = match engine.events(&task_id, cursor.as_deref(), 512).await {
            Ok(pair) => pair,
            Err(err) => {
                let (code, message) = split_coded(&err);
                write_line(stream, &Response::failure(id.clone(), code, message)).await?;
                return Ok(());
            }
        };
        let had_events = !events.is_empty();
        for record in &events {
            write_line(
                stream,
                &Response::stream_data(id.clone(), serde_json::to_value(record)?),
            )
            .await?;
        }
        cursor = Some(next);
        let (state, _) = *updates.borrow();
        if state.is_terminal() {
            return write_line(stream, &Response::stream_end(id)).await;
        }
        if !had_events {
            // Nothing new; wait for the next update (or client close).
            if updates.changed().await.is_err() {
                return write_line(stream, &Response::stream_end(id)).await;
            }
        }
    }
}

fn maybe_filter(events: Vec<hearth_agent_proto::task::EventRecord>, filter: Option<&str>) -> Value {
    use hearth_agent_proto::events::AgentEvent;
    let keep = |record: &hearth_agent_proto::task::EventRecord| match filter {
        None | Some("all") | Some("") => true,
        Some("assistant_text") => matches!(
            record.event,
            AgentEvent::TextMessageContent { .. }
                | AgentEvent::TextMessageStart { .. }
                | AgentEvent::TextMessageEnd { .. }
        ),
        Some("tool_summaries") => matches!(
            record.event,
            AgentEvent::ToolCallStart { .. } | AgentEvent::ToolCallResult { .. }
        ),
        Some(_) => true,
    };
    json!(events.into_iter().filter(|r| keep(r)).collect::<Vec<_>>())
}

fn str_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("request.invalid: missing string argument {key}"))
}

/// Our engine errors are `code: message`; split so the wire carries a real
/// error code. `TaskState` import keeps the terminal-check readable.
fn split_coded(err: &anyhow::Error) -> (String, String) {
    let text = err.to_string();
    match text.split_once(": ") {
        Some((code, message)) if !code.contains(' ') => (code.to_string(), message.to_string()),
        _ => ("task.error".to_string(), text),
    }
}

async fn write_line<S: AsyncWrite + Unpin>(stream: &mut S, response: &Response) -> Result<()> {
    stream
        .write_all((serde_json::to_string(response)? + "\n").as_bytes())
        .await?;
    stream.flush().await?;
    Ok(())
}
