//! The durable task registry (docs/agent-plane.md §3.3). Everything lives on
//! the guest disk: agentd restarts lose nothing, and a snapshot carries task
//! history with the VM.
//!
//! ```text
//! <state>/tasks/<task_id>/meta.toml      # identity, state, run history
//! <state>/tasks/<task_id>/events/NNNNNN.jsonl   # segmented AG-UI event log
//! <state>/tasks.index                    # task_id → state cache for listing
//! <state>/outbox/<delivery_id>.json      # §7.2 pending wake-ups
//! <state>/inbox-dedup/<delivery_id>      # §7.2 seen deliveries
//! ```

use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use hearth_agent_proto::events::AgentEvent;
use hearth_agent_proto::task::{
    Cursor, Delivery, EventRecord, Initiator, RunOutcome, RunRecord, TaskState, TaskSummary,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use ulid::Ulid;

/// On-disk task metadata. JSON-valued fields (`result`, `pending_input`) are
/// stored as JSON-encoded strings because TOML cannot hold arbitrary JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMeta {
    pub task_id: String,
    pub thread_id: String,
    pub agent: String,
    /// The adapter-native session/conversation id, once one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_thread: Option<String>,
    pub state: TaskState,
    pub incarnation: String,
    pub text: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_input_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<Initiator>,
    #[serde(default)]
    pub runs: Vec<RunRecord>,
}

impl TaskMeta {
    pub fn result(&self) -> Option<Value> {
        self.result_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
    }

    pub fn pending_input(&self) -> Option<Value> {
        self.pending_input_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
    }

    pub fn summary(&self, last_seq: u64) -> TaskSummary {
        TaskSummary {
            task_id: self.task_id.clone(),
            thread_id: self.thread_id.clone(),
            agent: self.agent.clone(),
            state: self.state,
            incarnation: self.incarnation.clone(),
            last_seq,
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            result: self.result(),
            pending_input: self.pending_input(),
            failure: self.failure.clone(),
            initiator: self.initiator.clone(),
            runs: self.runs.clone(),
        }
    }
}

/// The segmented event log for one task. Segments rotate at a byte cap and
/// retention drops whole oldest segments; a reader starting below the oldest
/// surviving seq gets a synthesized `CUSTOM hearth.truncation` event so a
/// stale cursor detects the gap instead of silently skipping (§3.3).
pub struct EventLog {
    dir: Utf8PathBuf,
    segment_max_bytes: u64,
    max_segments: usize,
    segments: Vec<u64>,
    active_size: u64,
    pub next_seq: u64,
    pub first_seq: u64,
}

impl EventLog {
    pub fn open(dir: &Utf8Path, segment_max_bytes: u64, max_segments: usize) -> Result<Self> {
        // At least one segment must survive a prune, or rotation empties the
        // segment list and the next append panics on `segments.last()`.
        let max_segments = max_segments.max(1);
        fs::create_dir_all(dir)?;
        let mut segments: Vec<u64> = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(number) = name
                .strip_suffix(".jsonl")
                .and_then(|stem| stem.parse::<u64>().ok())
            {
                segments.push(number);
            }
        }
        segments.sort_unstable();
        let mut log = Self {
            dir: dir.to_owned(),
            segment_max_bytes,
            max_segments,
            segments,
            active_size: 0,
            next_seq: 1,
            first_seq: 1,
        };
        if log.segments.is_empty() {
            log.segments.push(1);
        } else {
            if let Some(first) = log.first_record()? {
                log.first_seq = first.seq;
            }
            if let Some(last) = log.last_record()? {
                log.next_seq = last.seq + 1;
            }
            log.active_size = fs::metadata(log.segment_path(*log.segments.last().unwrap()))
                .map(|m| m.len())
                .unwrap_or(0);
        }
        Ok(log)
    }

    fn segment_path(&self, number: u64) -> Utf8PathBuf {
        self.dir.join(format!("{number:06}.jsonl"))
    }

    fn first_record(&self) -> Result<Option<EventRecord>> {
        let Some(first) = self.segments.first() else {
            return Ok(None);
        };
        let path = self.segment_path(*first);
        if !path.exists() {
            return Ok(None);
        }
        let file = fs::File::open(&path)?;
        let mut lines = BufReader::new(file).lines();
        match lines.next() {
            Some(line) => Ok(Some(serde_json::from_str(&line?)?)),
            None => Ok(None),
        }
    }

    fn last_record(&self) -> Result<Option<EventRecord>> {
        for number in self.segments.iter().rev() {
            let path = self.segment_path(*number);
            if !path.exists() {
                continue;
            }
            let file = fs::File::open(&path)?;
            let mut last = None;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if !line.trim().is_empty() {
                    last = Some(line);
                }
            }
            if let Some(line) = last {
                return Ok(Some(serde_json::from_str(&line)?));
            }
        }
        Ok(None)
    }

    /// Append one event; returns its seq.
    pub fn append(&mut self, run_id: &str, event: &AgentEvent) -> Result<u64> {
        let record = EventRecord {
            seq: self.next_seq,
            run_id: run_id.to_string(),
            event: event.clone(),
        };
        let line = serde_json::to_string(&record)? + "\n";
        let rotated =
            self.active_size + line.len() as u64 > self.segment_max_bytes && self.active_size > 0;
        if rotated {
            let next = self.segments.last().unwrap() + 1;
            self.segments.push(next);
            self.active_size = 0;
        }
        // Write the record into the active segment *before* pruning: prune
        // re-derives first_seq from the oldest surviving segment's first record,
        // which must exist on disk by then — otherwise (e.g. max_segments == 1,
        // where the just-rotated segment becomes the only survivor) first_seq
        // would stay stale and a truncated head would replay with no gap marker.
        let path = self.segment_path(*self.segments.last().unwrap());
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(line.as_bytes())?;
        self.active_size += line.len() as u64;
        self.next_seq += 1;
        if rotated {
            self.prune()?;
        }
        Ok(record.seq)
    }

    fn prune(&mut self) -> Result<()> {
        while self.segments.len() > self.max_segments {
            let oldest = self.segments.remove(0);
            let path = self.segment_path(oldest);
            let _ = fs::remove_file(path);
        }
        if let Some(first) = self.first_record()? {
            self.first_seq = first.seq;
        }
        Ok(())
    }

    /// Read events with `seq >= from`, up to `max`. A `from` below the oldest
    /// surviving seq yields a truncation marker first.
    pub fn read_from(&self, from: u64, max: usize) -> Result<Vec<EventRecord>> {
        let mut out = Vec::new();
        if from < self.first_seq {
            out.push(EventRecord {
                seq: from,
                run_id: String::new(),
                event: AgentEvent::Custom {
                    name: hearth_agent_proto::events::CUSTOM_TRUNCATION.to_string(),
                    value: serde_json::json!({
                        "requested_seq": from,
                        "first_available_seq": self.first_seq,
                    }),
                },
            });
        }
        for number in &self.segments {
            if out.len() >= max {
                break;
            }
            let path = self.segment_path(*number);
            if !path.exists() {
                continue;
            }
            let file = fs::File::open(&path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let record: EventRecord = serde_json::from_str(&line)?;
                if record.seq >= from {
                    out.push(record);
                    if out.len() >= max {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }
}

/// One task's on-disk presence.
pub struct Store {
    pub root: Utf8PathBuf,
    pub segment_max_bytes: u64,
    pub max_segments: usize,
}

impl Store {
    pub fn new(root: &Utf8Path, segment_max_bytes: u64, max_segments: usize) -> Result<Self> {
        for sub in ["tasks", "outbox", "inbox-dedup", "inbox"] {
            fs::create_dir_all(root.join(sub))?;
        }
        Ok(Self {
            root: root.to_owned(),
            segment_max_bytes,
            max_segments,
        })
    }

    pub fn task_dir(&self, task_id: &str) -> Utf8PathBuf {
        self.root.join("tasks").join(task_id)
    }

    pub fn write_meta(&self, meta: &TaskMeta) -> Result<()> {
        let dir = self.task_dir(&meta.task_id);
        fs::create_dir_all(&dir)?;
        let text = toml::to_string_pretty(meta)?;
        let tmp = dir.join(".meta.toml.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, dir.join("meta.toml"))?;
        Ok(())
    }

    pub fn read_meta(&self, task_id: &str) -> Result<TaskMeta> {
        let path = self.task_dir(task_id).join("meta.toml");
        let text = fs::read_to_string(&path)
            .with_context(|| format!("no task {task_id} ({path} unreadable)"))?;
        toml::from_str(&text).with_context(|| format!("parse {path}"))
    }

    pub fn open_log(&self, task_id: &str) -> Result<EventLog> {
        EventLog::open(
            &self.task_dir(task_id).join("events"),
            self.segment_max_bytes,
            self.max_segments,
        )
    }

    pub fn list_task_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(self.root.join("tasks"))? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                ids.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Rewrite `tasks.index` from metas — a listing cache, never authority.
    pub fn rewrite_index(&self, summaries: &[TaskSummary]) -> Result<()> {
        let mut text = String::new();
        for summary in summaries {
            text.push_str(&serde_json::to_string(&serde_json::json!({
                "task_id": summary.task_id,
                "thread_id": summary.thread_id,
                "agent": summary.agent,
                "state": summary.state,
                "updated_at": summary.updated_at,
            }))?);
            text.push('\n');
        }
        let tmp = self.root.join(".tasks.index.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, self.root.join("tasks.index"))?;
        Ok(())
    }

    pub fn remove_task(&self, task_id: &str) -> Result<()> {
        let dir = self.task_dir(task_id);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    // ---- outbox (§7.2) ----

    pub fn outbox_put(&self, delivery: &Delivery) -> Result<()> {
        let path = self
            .root
            .join("outbox")
            .join(format!("{}.json", delivery.delivery_id));
        let tmp = self.root.join("outbox").join(format!(
            ".{}.tmp",
            delivery.delivery_id
        ));
        // fsync the content before the atomic rename: an outbox entry is the
        // only durable record of a pending wake-up, so it must survive host
        // power loss, not just a process crash.
        write_sync(&tmp, &serde_json::to_vec(delivery)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn outbox_ack(&self, delivery_id: &str) -> Result<()> {
        validate_ulid(delivery_id)?;
        let path = self.root.join("outbox").join(format!("{delivery_id}.json"));
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    pub fn outbox_pending(&self) -> Result<Vec<Delivery>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("outbox"))? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            let text = fs::read_to_string(entry.path())?;
            match serde_json::from_str(&text) {
                Ok(delivery) => out.push(delivery),
                Err(err) => {
                    tracing::warn!(file = %name, error = %err, "unreadable outbox entry");
                }
            }
        }
        out.sort_by(|a: &Delivery, b: &Delivery| a.delivery_id.cmp(&b.delivery_id));
        Ok(out)
    }

    // ---- inbox: durable pending injections (§7.2) ----
    //
    // A wake-up is acked to agentd once `inject.turn` returns success, but the
    // injected turn then lives only in an in-memory queue. Persisting it here
    // first — and replaying on recover — means a guestd crash after the ack
    // still delivers the wake-up exactly once (the dedup set stops agentd's
    // retry; the inbox recovers a locally-lost injection).

    pub fn inbox_put(&self, delivery_id: &str, task_id: &str, text: &str) -> Result<()> {
        validate_ulid(delivery_id)?;
        let value = serde_json::json!({
            "delivery_id": delivery_id,
            "task_id": task_id,
            "text": text,
        });
        let path = self.root.join("inbox").join(format!("{delivery_id}.json"));
        let tmp = self.root.join("inbox").join(format!(".{delivery_id}.tmp"));
        write_sync(&tmp, &serde_json::to_vec(&value)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn inbox_ack(&self, delivery_id: &str) -> Result<()> {
        validate_ulid(delivery_id)?;
        let path = self.root.join("inbox").join(format!("{delivery_id}.json"));
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Pending injections to replay on boot: `(delivery_id, task_id, text)`.
    pub fn inbox_pending(&self) -> Result<Vec<(String, String, String)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("inbox"))? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            let text = fs::read_to_string(entry.path())?;
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let (Some(d), Some(t), Some(x)) = (
                    v.get("delivery_id").and_then(Value::as_str),
                    v.get("task_id").and_then(Value::as_str),
                    v.get("text").and_then(Value::as_str),
                ) {
                    out.push((d.to_string(), t.to_string(), x.to_string()));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    // ---- inbox dedup (§7.2) ----

    /// Record a delivery id; returns false if it was already seen (a retry
    /// that must be acked without re-injecting).
    pub fn dedup_insert(&self, delivery_id: &str) -> Result<bool> {
        validate_ulid(delivery_id)?;
        let path = self.root.join("inbox-dedup").join(delivery_id);
        match fs::OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(err) => Err(err.into()),
        }
    }
}

/// Write bytes to `path` and fsync them to disk before returning, so a
/// subsequent atomic rename yields a durable file even across host power loss.
fn write_sync(path: &Utf8Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn new_ulid() -> String {
    Ulid::new().to_string()
}

/// Delivery ids become file names; only accept ULID-shaped ones from peers.
fn validate_ulid(id: &str) -> Result<()> {
    if id.len() == 26 && id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        bail!("not a ULID: {id:?}")
    }
}

/// Parse and check a cursor against a task's incarnation (§3.4).
pub fn resolve_cursor(meta: &TaskMeta, cursor: Option<&str>) -> Result<u64> {
    match cursor {
        None | Some("") => Ok(1),
        Some(text) => {
            let cursor = Cursor::parse(text)
                .ok_or_else(|| anyhow!("cursor.malformed: unparseable cursor {text:?}"))?;
            if cursor.incarnation != meta.incarnation {
                bail!("cursor.stale: cursor predates a snapshot restore; re-sync via task.status");
            }
            Ok(cursor.seq + 1)
        }
    }
}

pub fn make_cursor(meta: &TaskMeta, seq: u64) -> String {
    Cursor {
        incarnation: meta.incarnation.clone(),
        seq,
    }
    .to_string()
}

/// Mark a task failed for a given reason (guestd restart, adapter loss).
pub fn fail_meta(meta: &mut TaskMeta, reason: &str) {
    meta.state = TaskState::Failed;
    meta.failure = Some(reason.to_string());
    meta.updated_at = now();
    if let Some(run) = meta.runs.last_mut() {
        if run.outcome.is_none() {
            run.outcome = Some(RunOutcome::Error);
            run.ended_at = Some(now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(event: &str) -> AgentEvent {
        AgentEvent::TextMessageContent {
            message_id: "m".into(),
            delta: event.into(),
        }
    }

    #[test]
    fn event_log_appends_rotates_prunes_and_marks_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8PathBuf::from_path_buf(tmp.path().join("events")).unwrap();
        // Tiny segments: every append rotates; keep 3 segments.
        let mut log = EventLog::open(&dir, 64, 3).unwrap();
        for i in 0..10 {
            log.append("r1", &record(&format!("delta-{i}"))).unwrap();
        }
        assert_eq!(log.next_seq, 11);
        assert!(log.first_seq > 1, "old segments must have been pruned");

        // A fresh open sees the same shape (restart survival).
        let reopened = EventLog::open(&dir, 64, 3).unwrap();
        assert_eq!(reopened.next_seq, 11);
        assert_eq!(reopened.first_seq, log.first_seq);

        // A stale cursor below the surviving head gets a truncation marker.
        let events = reopened.read_from(1, 100).unwrap();
        match &events[0].event {
            AgentEvent::Custom { name, .. } => {
                assert_eq!(name, hearth_agent_proto::events::CUSTOM_TRUNCATION)
            }
            other => panic!("expected truncation marker, got {other:?}"),
        }
        // And the rest replays exactly from the surviving head.
        assert_eq!(events[1].seq, reopened.first_seq);
        assert_eq!(events.last().unwrap().seq, 10);
    }

    // Regression: with a single surviving segment, the truncation marker and
    // first_seq must still be correct after rotation+prune (the write-before-
    // prune ordering). Previously first_seq stayed stale and the dropped head
    // replayed with no gap marker.
    #[test]
    fn single_segment_retention_marks_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8PathBuf::from_path_buf(tmp.path().join("events")).unwrap();
        let mut log = EventLog::open(&dir, 8, 1).unwrap();
        log.append("r", &record("first")).unwrap(); // seq 1
        log.append("r", &record("second")).unwrap(); // seq 2 → rotate, drop seg with seq 1
        assert_eq!(log.first_seq, 2, "first_seq tracks the surviving head");
        let events = log.read_from(1, 100).unwrap();
        match &events[0].event {
            AgentEvent::Custom { name, .. } => {
                assert_eq!(name, hearth_agent_proto::events::CUSTOM_TRUNCATION)
            }
            other => panic!("expected a truncation marker, got {other:?}"),
        }
        assert_eq!(events[1].seq, 2, "the surviving event replays after the marker");
        // The only entry at the dropped head's seq is the marker itself — the
        // original seq-1 record is never silently replayed as real content.
        let real_at_head = events
            .iter()
            .filter(|e| e.seq == 1)
            .any(|e| !matches!(&e.event, AgentEvent::Custom { name, .. }
                if name == hearth_agent_proto::events::CUSTOM_TRUNCATION));
        assert!(!real_at_head, "the dropped head must not replay as a real event");

        // max_segments is clamped to >= 1 so rotation never empties the list.
        let clamped = EventLog::open(&dir.join("z"), 8, 0).unwrap();
        assert_eq!(clamped.max_segments, 1);
    }

    #[test]
    fn cursor_resolution_enforces_incarnation() {
        let meta = TaskMeta {
            task_id: "t".into(),
            thread_id: "th".into(),
            agent: "codex".into(),
            native_thread: None,
            state: TaskState::Running,
            incarnation: "INC1".into(),
            text: String::new(),
            created_at: now(),
            updated_at: now(),
            result_json: None,
            pending_input_json: None,
            failure: None,
            initiator: None,
            runs: vec![],
        };
        assert_eq!(resolve_cursor(&meta, None).unwrap(), 1);
        assert_eq!(resolve_cursor(&meta, Some("INC1.41")).unwrap(), 42);
        let err = resolve_cursor(&meta, Some("OLD.41")).unwrap_err();
        assert!(err.to_string().contains("cursor.stale"));
    }

    #[test]
    fn outbox_and_dedup_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let store = Store::new(&root, 1024, 8).unwrap();
        let delivery = Delivery {
            delivery_id: new_ulid(),
            task_id: "t1".into(),
            transition: TaskState::Completed,
            detail: Some(serde_json::json!({"result": "ok"})),
            created: now(),
        };
        store.outbox_put(&delivery).unwrap();
        assert_eq!(store.outbox_pending().unwrap().len(), 1);
        store.outbox_ack(&delivery.delivery_id).unwrap();
        assert!(store.outbox_pending().unwrap().is_empty());
        // Acking twice is fine (at-least-once world).
        store.outbox_ack(&delivery.delivery_id).unwrap();

        assert!(store.dedup_insert(&delivery.delivery_id).unwrap());
        assert!(!store.dedup_insert(&delivery.delivery_id).unwrap());
        assert!(store.dedup_insert("../../etc/passwd").is_err());
    }

    // The durable inbox is what recovers an already-acked wake-up whose injected
    // run did not durably start before a crash (review finding: acked-before-
    // injection-durable). It survives a fresh Store open and is ack-idempotent.
    #[test]
    fn inbox_persists_pending_injections_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let id = new_ulid();
        {
            let store = Store::new(&root, 1024, 8).unwrap();
            store.inbox_put(&id, "task-1", "[hearth] wake up").unwrap();
            assert_eq!(store.inbox_pending().unwrap().len(), 1);
        }
        // A fresh open (a guestd restart) still sees the pending injection.
        let reopened = Store::new(&root, 1024, 8).unwrap();
        let pending = reopened.inbox_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, "task-1");
        assert_eq!(pending[0].2, "[hearth] wake up");
        reopened.inbox_ack(&id).unwrap();
        assert!(reopened.inbox_pending().unwrap().is_empty());
        reopened.inbox_ack(&id).unwrap(); // idempotent
        assert!(reopened.inbox_put("../evil", "t", "x").is_err());
    }
}
