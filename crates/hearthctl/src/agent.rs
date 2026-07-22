//! `hearthctl agent …` — the operator/UI client for hearth-agentd's control
//! socket (docs/agent-plane.md §10). Line-JSON over
//! `/run/hearth-agentd/agent.sock`, same framing as the machine plane, but with
//! the agent verb set.

use anyhow::{anyhow, bail, Result};
use camino::Utf8PathBuf;
use clap::Subcommand;
use comfy_table::{presets::UTF8_FULL, Table};
use hearth_agent_proto::{AgentRequest, AgentVerb};
use hearth_proto::{Response, StreamKind};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use ulid::Ulid;

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// List agent-enabled VMs, their adapters, and task counts.
    Ls,
    /// Start a task on an agent VM.
    Run {
        /// Target agent VM.
        agent: String,
        /// The task text.
        text: String,
    },
    /// List tasks across all agent VMs.
    Ps,
    /// Show one task's status.
    Status { task_ref: String },
    /// Read a task's event log from a cursor.
    Events {
        task_ref: String,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long, default_value_t = 256)]
        max: u64,
    },
    /// Answer a task that is awaiting input (starts a new run).
    Respond { task_ref: String, text: String },
    /// Continue a completed or failed task on the same conversation thread.
    Followup { task_ref: String, text: String },
    /// Cancel a task.
    Cancel { task_ref: String },
    /// Attach: replay from a cursor, then follow the event stream.
    Attach {
        task_ref: String,
        #[arg(long)]
        cursor: Option<String>,
    },
}

pub async fn run(socket: &Utf8PathBuf, command: &AgentCommand, json_out: bool) -> Result<()> {
    let (verb, args, streaming) = to_request(command);
    if streaming {
        return attach(socket, verb, args, json_out).await;
    }
    let value = request(socket, verb, args).await?;
    if json_out {
        println!("{}", serde_json::to_string(&value)?);
        return Ok(());
    }
    render(command, &value)
}

fn to_request(command: &AgentCommand) -> (AgentVerb, Map<String, Value>, bool) {
    match command {
        AgentCommand::Ls => (AgentVerb::AgentLs, Map::new(), false),
        AgentCommand::Run { agent, text } => (
            AgentVerb::TaskStart,
            args([("agent", json!(agent)), ("text", json!(text))]),
            false,
        ),
        AgentCommand::Ps => (AgentVerb::TaskList, Map::new(), false),
        AgentCommand::Status { task_ref } => (
            AgentVerb::TaskStatus,
            args([("task_ref", json!(task_ref))]),
            false,
        ),
        AgentCommand::Events {
            task_ref,
            cursor,
            filter,
            max,
        } => {
            let mut a = args([("task_ref", json!(task_ref)), ("max", json!(max))]);
            if let Some(cursor) = cursor {
                a.insert("cursor".into(), json!(cursor));
            }
            if let Some(filter) = filter {
                a.insert("filter".into(), json!(filter));
            }
            (AgentVerb::TaskEvents, a, false)
        }
        AgentCommand::Respond { task_ref, text } => (
            AgentVerb::TaskRespond,
            args([
                ("task_ref", json!(task_ref)),
                ("response", json!({ "text": text })),
            ]),
            false,
        ),
        AgentCommand::Followup { task_ref, text } => (
            AgentVerb::TaskFollowup,
            args([("task_ref", json!(task_ref)), ("text", json!(text))]),
            false,
        ),
        AgentCommand::Cancel { task_ref } => (
            AgentVerb::TaskCancel,
            args([("task_ref", json!(task_ref))]),
            false,
        ),
        AgentCommand::Attach { task_ref, cursor } => {
            let mut a = args([("task_ref", json!(task_ref))]);
            if let Some(cursor) = cursor {
                a.insert("cursor".into(), json!(cursor));
            }
            (AgentVerb::TaskAttach, a, true)
        }
    }
}

async fn request(socket: &Utf8PathBuf, verb: AgentVerb, args: Map<String, Value>) -> Result<Value> {
    let stream = UnixStream::connect(socket.as_str())
        .await
        .map_err(|e| anyhow!("connect agentd socket {socket}: {e}"))?;
    let (read, mut write) = stream.into_split();
    let req = AgentRequest::new(Ulid::new().to_string(), verb, args);
    write
        .write_all((serde_json::to_string(&req)? + "\n").as_bytes())
        .await?;
    write.shutdown().await?;
    let mut lines = BufReader::new(read).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("agentd closed without a response"))?;
    let resp: Response = serde_json::from_str(&line)?;
    if resp.ok {
        Ok(resp.result.unwrap_or(Value::Null))
    } else {
        let err = resp.error.ok_or_else(|| anyhow!("unknown agentd error"))?;
        bail!("{}: {}", err.code, err.message)
    }
}

async fn attach(
    socket: &Utf8PathBuf,
    verb: AgentVerb,
    args: Map<String, Value>,
    json_out: bool,
) -> Result<()> {
    let stream = UnixStream::connect(socket.as_str())
        .await
        .map_err(|e| anyhow!("connect agentd socket {socket}: {e}"))?;
    let (read, mut write) = stream.into_split();
    let req = AgentRequest::new(Ulid::new().to_string(), verb, args);
    write
        .write_all((serde_json::to_string(&req)? + "\n").as_bytes())
        .await?;
    write.shutdown().await?;
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let resp: Response = serde_json::from_str(&line)?;
        if !resp.ok {
            let err = resp.error.map(|e| format!("{}: {}", e.code, e.message));
            bail!("attach failed: {}", err.unwrap_or_default());
        }
        if resp.stream == Some(StreamKind::End) {
            break;
        }
        if let Some(frame) = resp.result {
            if json_out {
                println!("{}", serde_json::to_string(&frame)?);
            } else {
                print_event_frame(&frame);
            }
        }
    }
    Ok(())
}

fn render(command: &AgentCommand, value: &Value) -> Result<()> {
    match command {
        AgentCommand::Ls => render_agents(value),
        AgentCommand::Ps => render_tasks(value),
        AgentCommand::Events { .. } => {
            for event in value
                .get("events")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                print_event_frame(event);
            }
            if let Some(cursor) = value.get("cursor").and_then(Value::as_str) {
                println!("cursor: {cursor}");
            }
            Ok(())
        }
        AgentCommand::Run { .. } | AgentCommand::Respond { .. } | AgentCommand::Followup { .. } => {
            if let Some(task_ref) = value.get("task_ref").and_then(Value::as_str) {
                println!("task_ref: {task_ref}");
            }
            if let Some(state) = value.get("state") {
                println!("state: {state}");
            }
            Ok(())
        }
        _ => {
            println!("{}", serde_json::to_string_pretty(value)?);
            Ok(())
        }
    }
}

fn render_agents(value: &Value) -> Result<()> {
    let agents = value
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("malformed agent ls response"))?;
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(["NAME", "RUNNING", "READY", "ADAPTERS", "TASKS"]);
    for agent in agents {
        let adapters = agent
            .get("adapters")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        table.add_row([
            cell(agent, "hostname"),
            cell(agent, "running"),
            cell(agent, "ready"),
            adapters,
            cell(agent, "task_count"),
        ]);
    }
    println!("{table}");
    Ok(())
}

fn render_tasks(value: &Value) -> Result<()> {
    let tasks = value
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("malformed task list response"))?;
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(["TASK", "AGENT_VM", "STATE", "UPDATED"]);
    for task in tasks {
        table.add_row([
            cell(task, "task_id"),
            cell(task, "agent_vm"),
            cell(task, "state"),
            cell(task, "updated_at"),
        ]);
    }
    println!("{table}");
    Ok(())
}

fn print_event_frame(frame: &Value) {
    let event = frame.get("event").unwrap_or(frame);
    let seq = frame.get("seq").and_then(Value::as_u64);
    let ty = event.get("type").and_then(Value::as_str).unwrap_or("?");
    match seq {
        Some(seq) => println!("[{seq:>4}] {ty} {}", compact(event)),
        None => println!("{ty} {}", compact(event)),
    }
}

fn compact(event: &Value) -> String {
    match event.get("type").and_then(Value::as_str) {
        Some("TEXT_MESSAGE_CONTENT") => event
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Some("CUSTOM") => format!(
            "{} {}",
            event.get("name").and_then(Value::as_str).unwrap_or(""),
            event
                .get("value")
                .map(|v| v.to_string())
                .unwrap_or_default()
        ),
        _ => serde_json::to_string(event).unwrap_or_default(),
    }
}

fn cell(value: &Value, key: &str) -> String {
    value
        .get(key)
        .map(|v| {
            v.as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default()
}

fn args<const N: usize>(items: [(&str, Value); N]) -> Map<String, Value> {
    items.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}
