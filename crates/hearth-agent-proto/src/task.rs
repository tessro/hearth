//! The task/thread/run triad (§3.1) and the records both ends of the task
//! verbs exchange. Deliberately isomorphic to A2A's task model (§3.2) so a
//! future façade is translation, not redesign.

use crate::events::AgentEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    AwaitingInput,
    Completed,
    Failed,
    Canceled,
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Canceled
        )
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TaskState::Queued => "queued",
            TaskState::Running => "running",
            TaskState::AwaitingInput => "awaiting_input",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Canceled => "canceled",
        };
        f.write_str(s)
    }
}

/// Runs are terminal per AG-UI's interrupt lifecycle: finished, error, or
/// interrupted — never reopened (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Finished,
    Error,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

/// Who started a task. Inside the guest this is an advisory copy for
/// debugging; wake-up authority always comes from agentd's ledger (§4.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Initiator {
    /// "ui" | "agent" | "local"
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// One event as stored in (and replayed from) the guest event log. `seq` is
/// monotonic per task across runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub seq: u64,
    pub run_id: String,
    pub event: AgentEvent,
}

/// A replay cursor: `(incarnation, seq)` rendered as `"<incarnation>.<seq>"`.
/// The incarnation rotates on snapshot restore, staling every outstanding
/// cursor instead of silently re-issuing seqs for different events (§3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub incarnation: String,
    pub seq: u64,
}

impl Cursor {
    pub fn parse(text: &str) -> Option<Self> {
        let (incarnation, seq) = text.rsplit_once('.')?;
        if incarnation.is_empty() {
            return None;
        }
        Some(Self {
            incarnation: incarnation.to_string(),
            seq: seq.parse().ok()?,
        })
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.incarnation, self.seq)
    }
}

/// Task summary as returned by `task.status` / `task.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub thread_id: String,
    pub agent: String,
    pub state: TaskState,
    pub incarnation: String,
    /// Seq of the newest event, so callers can build a follow cursor.
    pub last_seq: u64,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Set when state is `awaiting_input`: the pending prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<Initiator>,
    #[serde(default)]
    pub runs: Vec<RunRecord>,
}

/// One durable wake-up (§7.2): persisted in the callee's outbox until agentd
/// acks it, delivered to the initiator's guestd idempotently by `delivery_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delivery {
    pub delivery_id: String,
    pub task_id: String,
    /// The task state this delivery reports.
    pub transition: TaskState,
    /// Human-readable payload (permission prompt, result summary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    pub created: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let cursor = Cursor {
            incarnation: "01J5KAAAAAAAAAAAAAAAAAAAAA".into(),
            seq: 141,
        };
        let text = cursor.to_string();
        assert_eq!(Cursor::parse(&text), Some(cursor));
        assert_eq!(Cursor::parse("no-seq"), None);
        assert_eq!(Cursor::parse(".5"), None);
        assert_eq!(Cursor::parse("inc.notanumber"), None);
    }

    #[test]
    fn task_states_use_a2a_isomorphic_wire_names() {
        assert_eq!(
            serde_json::to_string(&TaskState::AwaitingInput).unwrap(),
            "\"awaiting_input\""
        );
        assert!(TaskState::Completed.is_terminal());
        assert!(!TaskState::AwaitingInput.is_terminal());
    }
}
