//! The AG-UI HTTP leg (docs/agent-plane.md §4.2). A hand-rolled HTTP/1.1 + SSE
//! server — the ecosystem SDKs are TS/Python, and the surface is small. Two
//! kinds of endpoint:
//!
//! - `POST /v1/agents/{name}/agui` — **standard AG-UI**: accepts a
//!   `RunAgentInput`, streams `BaseEvent`s over SSE. A fresh `threadId` creates
//!   a task; `forwardedProps.task_ref` resumes an approval or continues a
//!   settled task as a new run on the same thread. An unmodified AG-UI
//!   `HttpAgent` drives this (Phase 3's conformance bar).
//! - Hearth task API (honestly namespaced extensions): `GET /v1/agents`,
//!   `GET /v1/tasks`, `GET /v1/tasks/{ref}`, `GET /v1/tasks/{ref}/events` (SSE),
//!   `POST /v1/tasks/{ref}/cancel`.
//!
//! Auth: a bearer token (via `LoadCredential=`) is required end-to-end; browser
//! origins need an explicit CORS allowlist. The bind is never `0.0.0.0`
//! silently — this surface drives code-executing agents and is treated like an
//! SSH key.

use crate::core::Agentd;
use crate::relay;
use anyhow::{anyhow, Context, Result};
use hearth_agent_proto::hmac::constant_time_eq;
use hearth_agent_proto::AgentVerb;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::warn;

pub struct HttpConfig {
    pub token: Vec<u8>,
    pub cors_origins: Vec<String>,
}

pub async fn serve(
    agentd: Arc<Agentd>,
    listener: TcpListener,
    http: Arc<HttpConfig>,
) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let agentd = Arc::clone(&agentd);
        let http = Arc::clone(&http);
        tokio::spawn(async move {
            if let Err(err) = handle(agentd, http, stream).await {
                warn!(error = %err, "http connection failed");
            }
        });
    }
}

struct HttpRequest {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<Option<HttpRequest>> {
    // Read headers up to CRLFCRLF.
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.push(byte[0]);
        if buf.len() > 64 * 1024 {
            return Err(anyhow!("request headers too large"));
        }
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; content_length.min(8 * 1024 * 1024)];
    if !body.is_empty() {
        stream.read_exact(&mut body).await?;
    }
    Ok(Some(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    }))
}

async fn handle(agentd: Arc<Agentd>, http: Arc<HttpConfig>, mut stream: TcpStream) -> Result<()> {
    let Some(req) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let origin = req.header("origin").map(str::to_string);
    let cors = cors_header(&http, origin.as_deref());

    // CORS preflight.
    if req.method == "OPTIONS" {
        return write_response(&mut stream, 204, "No Content", &cors, b"").await;
    }

    // Auth: bearer token required on every route.
    if !authorized(&http, &req) {
        return write_json(
            &mut stream,
            401,
            "Unauthorized",
            &cors,
            &json!({ "error": "unauthorized" }),
        )
        .await;
    }

    let result = route(&agentd, &req, &mut stream, &cors).await;
    if let Err(err) = result {
        // If we already started streaming, the connection just closes.
        warn!(path = %req.path, error = %err, "http route error");
        let _ = write_json(
            &mut stream,
            500,
            "Internal Server Error",
            &cors,
            &json!({ "error": err.to_string() }),
        )
        .await;
    }
    Ok(())
}

async fn route(
    agentd: &Arc<Agentd>,
    req: &HttpRequest,
    stream: &mut TcpStream,
    cors: &[(String, String)],
) -> Result<()> {
    let segments: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    match (req.method.as_str(), segments.as_slice()) {
        ("GET", ["v1", "agents"]) => {
            let agents = agentd.list_agents().await?;
            write_json(stream, 200, "OK", cors, &agents).await
        }
        ("POST", ["v1", "agents", name, "agui"]) => agui_run(agentd, req, stream, cors, name).await,
        ("GET", ["v1", "tasks"]) => {
            let tasks = list_tasks(agentd).await?;
            write_json(stream, 200, "OK", cors, &tasks).await
        }
        ("GET", ["v1", "tasks", task_ref]) => {
            let claims = agentd.resolve_ref(task_ref, "ui")?;
            let status = agentd
                .relay_verb(&claims, AgentVerb::TaskStatus, Map::new())
                .await?;
            write_json(stream, 200, "OK", cors, &status).await
        }
        ("GET", ["v1", "tasks", task_ref, "events"]) => {
            events_sse(agentd, req, stream, cors, task_ref).await
        }
        ("POST", ["v1", "tasks", task_ref, "cancel"]) => {
            let claims = agentd.resolve_ref(task_ref, "ui")?;
            let result = agentd
                .relay_verb(&claims, AgentVerb::TaskCancel, Map::new())
                .await?;
            agentd.cancel_grant(&claims.task_id)?;
            write_json(stream, 200, "OK", cors, &result).await
        }
        _ => {
            write_json(
                stream,
                404,
                "Not Found",
                cors,
                &json!({ "error": "not found" }),
            )
            .await
        }
    }
}

/// The AG-UI run endpoint (§4.2). Creates or resumes a task, then streams the
/// resulting run's events as SSE `BaseEvent`s until the run finishes.
async fn agui_run(
    agentd: &Arc<Agentd>,
    req: &HttpRequest,
    stream: &mut TcpStream,
    cors: &[(String, String)],
    agent_vm: &str,
) -> Result<()> {
    let requested_id = agentd
        .hearthd
        .agent_endpoints()
        .await?
        .into_iter()
        .find(|endpoint| endpoint.hostname == agent_vm)
        .map(|endpoint| endpoint.id)
        .ok_or_else(|| anyhow!("agent.not_enabled: {agent_vm:?} is not an agent-enabled VM"))?;
    let input: Value = serde_json::from_slice(&req.body).context("parse RunAgentInput")?;
    let messages = input.get("messages").and_then(Value::as_array);
    let user_text = messages
        .and_then(|msgs| {
            msgs.iter()
                .rev()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        })
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let task_ref = input
        .get("forwardedProps")
        .and_then(|p| p.get("task_ref"))
        .and_then(Value::as_str)
        .map(str::to_string);

    // Create or resume.
    let (claims, from_cursor) = match task_ref {
        None => {
            // Fresh thread → new task.
            let created = agentd.delegate_from_ui("ui", agent_vm, &user_text).await?;
            let ref_token = created
                .get("task_ref")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("no task_ref from create"))?;
            let claims = agentd.resolve_ref(ref_token, "ui")?;
            (claims, None)
        }
        Some(token) => {
            let claims = agentd.resolve_ref(&token, "ui")?;
            if claims.target != requested_id {
                return Err(anyhow!(
                    "task.target_mismatch: task belongs to {:?}, not {:?}",
                    claims.target,
                    agent_vm
                ));
            }
            // Capture the current tip BEFORE continuing so we stream every
            // event of the new run and none of the old (no gap, no dup).
            let status = agentd
                .relay_verb(&claims, AgentVerb::TaskStatus, Map::new())
                .await?;
            let cursor = status
                .get("incarnation")
                .and_then(Value::as_str)
                .zip(status.get("last_seq").and_then(Value::as_u64))
                .map(|(inc, seq)| format!("{inc}.{seq}"));
            match status.get("state").and_then(Value::as_str) {
                Some("awaiting_input") => {
                    let mut extra = Map::new();
                    extra.insert("response".to_string(), json!({ "text": user_text }));
                    agentd
                        .relay_verb(&claims, AgentVerb::TaskRespond, extra)
                        .await?;
                }
                Some("completed" | "failed") => {
                    let mut extra = Map::new();
                    extra.insert("text".to_string(), json!(user_text));
                    agentd
                        .relay_verb(&claims, AgentVerb::TaskFollowup, extra)
                        .await?;
                }
                Some(state) => {
                    return Err(anyhow!(
                        "task.not_settled: task is {state} and cannot accept another turn"
                    ));
                }
                None => return Err(anyhow!("task.invalid_status: task state is missing")),
            }
            (claims, cursor)
        }
    };

    // Begin SSE.
    write_sse_head(stream, cors).await?;
    let ref_token = agentd.mint_ref(
        &claims.target,
        &claims.task_id,
        "ui",
        claims.initiator_thread.as_deref(),
    );

    // Attach to the guest and forward AG-UI events until the run ends.
    let mut extra = Map::new();
    if let Some(cursor) = &from_cursor {
        extra.insert("cursor".to_string(), json!(cursor));
    }
    extra.insert("task_id".to_string(), json!(claims.task_id));
    let (mut guest, _) = relay::attach(&agentd.hearthd, &claims.target, extra).await?;
    loop {
        match relay::next_attach_frame(&mut guest).await {
            Ok(Some(frame)) => {
                let event = frame.get("event").cloned().unwrap_or(Value::Null);
                if is_run_end(&event) {
                    // Hand the client the ref it needs to resume before the
                    // terminal event. AG-UI requires RUN_FINISHED/RUN_ERROR
                    // to be the final typed event in a run.
                    let ref_event = json!({
                        "type": "CUSTOM",
                        "name": "hearth.task_ref",
                        "value": { "task_ref": ref_token },
                    });
                    write_sse_event(stream, &ref_event).await?;
                    write_sse_event(stream, &event).await?;
                    break;
                }
                write_sse_event(stream, &event).await?;
            }
            Ok(None) => break,
            Err(err) => {
                warn!(error = %err, "agui attach dropped");
                break;
            }
        }
    }
    stream.flush().await?;
    Ok(())
}

/// Exact replay from a cursor, then follow (§4.2 task events SSE).
async fn events_sse(
    agentd: &Arc<Agentd>,
    req: &HttpRequest,
    stream: &mut TcpStream,
    cors: &[(String, String)],
    task_ref: &str,
) -> Result<()> {
    let claims = agentd.resolve_ref(task_ref, "ui")?;
    let cursor = query_param(&req.query, "cursor");
    write_sse_head(stream, cors).await?;
    let mut extra = Map::new();
    if let Some(cursor) = cursor {
        extra.insert("cursor".to_string(), json!(cursor));
    }
    extra.insert("task_id".to_string(), json!(claims.task_id));
    let (mut guest, _) = relay::attach(&agentd.hearthd, &claims.target, extra).await?;
    loop {
        match relay::next_attach_frame(&mut guest).await {
            Ok(Some(frame)) => write_sse_event(stream, &frame).await?,
            Ok(None) => break,
            Err(_) => break,
        }
    }
    stream.write_all(b"event: done\ndata: {}\n\n").await?;
    stream.flush().await?;
    Ok(())
}

async fn list_tasks(agentd: &Arc<Agentd>) -> Result<Value> {
    let mut all = Vec::new();
    for endpoint in agentd.hearthd.agent_endpoints().await? {
        if !endpoint.running {
            continue;
        }
        if let Ok(value) = relay::call(
            &agentd.hearthd,
            &endpoint.id,
            AgentVerb::TaskList,
            Map::new(),
        )
        .await
        {
            if let Some(tasks) = value.get("tasks").and_then(Value::as_array) {
                for task in tasks {
                    let mut task = task.clone();
                    task["agent_id"] = json!(endpoint.id);
                    task["agent_hostname"] = json!(endpoint.hostname);
                    if let Some(task_id) = task.get("task_id").and_then(Value::as_str) {
                        let task_ref = agentd.mint_ref(&endpoint.id, task_id, "ui", None);
                        task["task_ref"] = json!(task_ref);
                    }
                    all.push(task);
                }
            }
        }
    }
    Ok(json!({ "tasks": all }))
}

fn is_run_end(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("RUN_FINISHED") | Some("RUN_ERROR")
    )
}

fn authorized(http: &HttpConfig, req: &HttpRequest) -> bool {
    let Some(auth) = req.header("authorization") else {
        return false;
    };
    let Some(token) = auth.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(token.trim().as_bytes(), &http.token)
}

fn cors_header(http: &HttpConfig, origin: Option<&str>) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    if let Some(origin) = origin {
        if http.cors_origins.iter().any(|o| o == origin) {
            headers.push((
                "Access-Control-Allow-Origin".to_string(),
                origin.to_string(),
            ));
            headers.push((
                "Access-Control-Allow-Headers".to_string(),
                "authorization, content-type".to_string(),
            ));
            headers.push((
                "Access-Control-Allow-Methods".to_string(),
                "GET, POST, OPTIONS".to_string(),
            ));
        }
    }
    headers
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| urldecode(v))
    })
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn write_sse_head(stream: &mut TcpStream, cors: &[(String, String)]) -> Result<()> {
    let mut head = String::from("HTTP/1.1 200 OK\r\n");
    head.push_str("Content-Type: text/event-stream\r\n");
    head.push_str("Cache-Control: no-cache\r\n");
    head.push_str("Connection: close\r\n");
    for (k, v) in cors {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn write_sse_event(stream: &mut TcpStream, event: &Value) -> Result<()> {
    let line = format!("data: {}\n\n", serde_json::to_string(event)?);
    stream.write_all(line.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn write_json(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    cors: &[(String, String)],
    body: &Value,
) -> Result<()> {
    let bytes = serde_json::to_vec(body)?;
    let extra: Vec<(String, String)> =
        std::iter::once(("Content-Type".to_string(), "application/json".to_string()))
            .chain(cors.iter().cloned())
            .collect();
    write_response(stream, code, reason, &extra, &bytes).await
}

async fn write_response(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<()> {
    let mut head = format!("HTTP/1.1 {code} {reason}\r\n");
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str("Connection: close\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}
