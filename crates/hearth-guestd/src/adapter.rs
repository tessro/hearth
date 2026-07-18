//! Agent adapters (docs/agent-plane.md §2.2). One adapter per CLI, translating
//! its native stream into the AG-UI event vocabulary and mapping the triad
//! (task/thread/run) onto its native concepts. Codex goes first: `codex
//! app-server` is the best-specified of the three (§2.2).
//!
//! The adapter is a trait so guestd's task engine is CLI-agnostic and the e2e
//! tests can drive a fake CLI that speaks the same pinned contract without a
//! real codex binary in the loop.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hearth_agent_proto::events::AgentEvent;
use serde_json::Value;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

pub mod claude;
pub mod codex;
pub mod hermes;

/// What an adapter emits as it drives one run. `Event` is appended to the log;
/// `AwaitingInput` ends the current run `interrupted` and moves the task to
/// `awaiting_input` carrying the prompt; `Finished`/`Failed` end the run and
/// the task terminally.
#[derive(Debug, Clone)]
pub enum AdapterEvent {
    Event(AgentEvent),
    AwaitingInput { prompt: Value },
    Finished { result: Value },
    Failed { message: String },
}

/// Live delivery path for ordinary AG-UI events. Adapters send message,
/// reasoning, tool, and RAW events here as soon as their native protocol emits
/// them; the engine persists them while the adapter is still running. Terminal
/// task transitions remain in `RunOutput` so the native session id is durable
/// before a run is finalized.
#[derive(Clone)]
pub struct EventSink {
    sender: UnboundedSender<AgentEvent>,
}

impl EventSink {
    pub fn channel() -> (Self, UnboundedReceiver<AgentEvent>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self { sender }, receiver)
    }

    pub fn emit(&self, event: AgentEvent) -> Result<()> {
        self.sender
            .send(event)
            .map_err(|_| anyhow!("agent event stream closed while the adapter was running"))
    }
}

/// Flush ordinary events from an adapter's translation buffer without moving
/// terminal/approval control events out of the run result.
pub fn flush_events(events: &mut Vec<AdapterEvent>, sink: &EventSink) -> Result<()> {
    let mut control = Vec::new();
    for event in std::mem::take(events) {
        match event {
            AdapterEvent::Event(event) => sink.emit(event)?,
            event => control.push(event),
        }
    }
    *events = control;
    Ok(())
}

/// A driven run's terminal/approval events, plus the native session id the CLI
/// assigned (so the next run resumes the same conversation). Ordinary AG-UI
/// events travel through `EventSink` while this result is still pending.
pub struct RunOutput {
    pub events: Vec<AdapterEvent>,
    pub native_thread: Option<String>,
}

/// One agent CLI. Adapters couple to CLI stream formats, which drift, so the
/// CLI is pinned by version in the image and the adapter refuses (loudly, at
/// boot report) a version it does not know (§2.2).
#[async_trait]
pub trait Adapter: Send + Sync {
    fn name(&self) -> &str;

    /// Probe the CLI and report its version, or an error explaining why this
    /// adapter refuses to drive it. Surfaces in the boot report (§2.1).
    async fn probe(&self) -> Result<String>;

    /// Start a new run. `thread_id` is Hearth's durable thread identity (used
    /// to bind per-session MCP shims); `native_thread` is the CLI-assigned
    /// session id, absent on the first run. `input` is the task text or resume
    /// payload.
    async fn run(
        &self,
        thread_id: &str,
        native_thread: Option<&str>,
        input: &Value,
        events: EventSink,
    ) -> Result<RunOutput>;
}
