//! Relaying task verbs to a guestd over the hearthd broker (docs/agent-plane.md
//! §4). agentd is content-stateless: it holds no task state, just forwards a
//! verb to the right guest (resolved from a signed ref or an explicit vm) and
//! returns the guest's answer.

use crate::hearthd::Hearthd;
use anyhow::{anyhow, bail, Context, Result};
use hearth_agent_proto::{read_line_capped, AgentRequest, AgentVerb, MAX_LINE_BYTES};
use hearth_proto::{Response, StreamKind};
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use ulid::Ulid;

/// One task verb, one response, against a guest's task-verb server.
pub async fn call(
    hearthd: &Hearthd,
    vm: &str,
    verb: AgentVerb,
    args: Map<String, Value>,
) -> Result<Value> {
    let mut stream = hearthd.guest_connect(vm).await?;
    let req = AgentRequest::new(Ulid::new().to_string(), verb, args);
    write_request(&mut stream, &req).await?;
    let resp = read_one(&mut stream).await?;
    into_result(resp)
}

/// Open a streaming attach against a guest and return the connected stream
/// plus the initial request id, so the caller can pump frames.
pub async fn attach(
    hearthd: &Hearthd,
    vm: &str,
    args: Map<String, Value>,
) -> Result<(UnixStream, String)> {
    let mut stream = hearthd.guest_connect(vm).await?;
    let id = Ulid::new().to_string();
    let req = AgentRequest::new(id.clone(), AgentVerb::TaskAttach, args);
    write_request(&mut stream, &req).await?;
    Ok((stream, id))
}

/// Read one non-stream response, mapping ok/err into a Result.
async fn read_one(stream: &mut UnixStream) -> Result<Response> {
    let line = read_line_capped(stream, MAX_LINE_BYTES)
        .await?
        .ok_or_else(|| anyhow!("guestd closed without a response"))?;
    serde_json::from_str(&line).context("parse guestd response")
}

pub fn into_result(resp: Response) -> Result<Value> {
    if resp.ok {
        Ok(resp.result.unwrap_or(Value::Null))
    } else {
        let err = resp
            .error
            .ok_or_else(|| anyhow!("guestd error without body"))?;
        bail!("{}: {}", err.code, err.message)
    }
}

/// Read the next attach frame: `Some(record)` for a data frame, `None` at the
/// stream end.
pub async fn next_attach_frame(stream: &mut UnixStream) -> Result<Option<Value>> {
    loop {
        let Some(line) = read_line_capped(stream, MAX_LINE_BYTES).await? else {
            return Ok(None);
        };
        if line.trim().is_empty() {
            continue;
        }
        let resp: Response = serde_json::from_str(&line).context("parse attach frame")?;
        if !resp.ok {
            let err = resp.error.map(|e| format!("{}: {}", e.code, e.message));
            bail!("attach error: {}", err.unwrap_or_default());
        }
        match resp.stream {
            Some(StreamKind::End) => return Ok(None),
            _ => return Ok(Some(resp.result.unwrap_or(Value::Null))),
        }
    }
}

async fn write_request(stream: &mut UnixStream, req: &AgentRequest) -> Result<()> {
    stream
        .write_all((serde_json::to_string(req)? + "\n").as_bytes())
        .await?;
    Ok(())
}
