//! Agent adapters (docs/agent-plane.md §2.2). One adapter per CLI, translating
//! its native stream into the AG-UI event vocabulary and mapping the triad
//! (task/thread/run) onto its native concepts. Codex goes first: `codex
//! app-server` is the best-specified of the three (§2.2).
//!
//! The adapter is a trait so guestd's task engine is CLI-agnostic and the e2e
//! tests can drive a fake CLI that speaks the same pinned contract without a
//! real codex binary in the loop.

use anyhow::Result;
use async_trait::async_trait;
use hearth_agent_proto::events::AgentEvent;
use serde_json::Value;

pub mod claude;
pub mod codex;

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

/// A driven run: the ordered adapter events, plus the native session id the
/// CLI assigned (so the next run resumes the same conversation).
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

    /// Start a new run. `native_thread` is `None` for the task's first run and
    /// the CLI-assigned session id for a resume. `input` is the task text (first
    /// run) or the resume payload (answering `awaiting_input`).
    async fn run(&self, native_thread: Option<&str>, input: &Value) -> Result<RunOutput>;
}
