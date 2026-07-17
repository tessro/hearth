//! The task engine: owns the durable registry, drives adapters, serializes
//! turns per thread, and enqueues durable wake-ups. This is the heart of
//! guestd's agent-plane duties (docs/agent-plane.md §3, §2.3, §7.2).
//!
//! Concurrency model (day-1, §14): one active run per thread. `enqueue_run`
//! appends to the thread's turn queue and ensures exactly one driver task is
//! draining it; you cannot inject into a running turn, so wake-ups and
//! responses queue until the current run ends and then enter as new runs.

use crate::adapter::{Adapter, AdapterEvent};
use crate::store::{fail_meta, make_cursor, new_ulid, now, resolve_cursor, Store, TaskMeta};
use anyhow::{anyhow, bail, Result};
use hearth_agent_proto::events::AgentEvent;
use hearth_agent_proto::task::{
    Delivery, EventRecord, Initiator, RunOutcome, RunRecord, TaskState, TaskSummary,
};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{watch, Notify};

struct TaskCell {
    meta: Mutex<TaskMeta>,
    log: tokio::sync::Mutex<crate::store::EventLog>,
    /// (state, last_seq) for followers/long-polls.
    update_tx: watch::Sender<(TaskState, u64)>,
    /// Serializes runs on this task's thread.
    thread_lock: Arc<tokio::sync::Mutex<()>>,
    queue: Mutex<VecDeque<Value>>,
    driving: AtomicBool,
}

pub struct Engine {
    pub store: Arc<Store>,
    adapters: HashMap<String, Arc<dyn Adapter>>,
    cells: Mutex<HashMap<String, Arc<TaskCell>>>,
    thread_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// thread_id → task_id, so `inject.turn` can route a wake-up by thread.
    thread_index: Mutex<HashMap<String, String>>,
    incarnation: Mutex<String>,
    /// Bumped whenever a new outbox entry lands, to nudge the upcall loop.
    pub outbox_notify: Arc<Notify>,
}

impl Engine {
    pub fn new(store: Arc<Store>, adapters: HashMap<String, Arc<dyn Adapter>>) -> Arc<Self> {
        Arc::new(Self {
            store,
            adapters,
            cells: Mutex::new(HashMap::new()),
            thread_locks: Mutex::new(HashMap::new()),
            thread_index: Mutex::new(HashMap::new()),
            incarnation: Mutex::new(new_ulid()),
            outbox_notify: Arc::new(Notify::new()),
        })
    }

    /// Load persisted tasks on boot. Any task left `running`/`queued` when
    /// guestd died had its in-flight turn lost: mark it `failed(guestd_restart)`
    /// with a marker event (§9). Terminal and awaiting_input tasks survive
    /// unchanged.
    pub async fn recover(self: &Arc<Self>) -> Result<()> {
        for task_id in self.store.list_task_ids()? {
            let mut meta = match self.store.read_meta(&task_id) {
                Ok(meta) => meta,
                Err(err) => {
                    tracing::warn!(task = %task_id, error = %err, "skipping unreadable task");
                    continue;
                }
            };
            let mut log = self.store.open_log(&task_id)?;
            if matches!(meta.state, TaskState::Running | TaskState::Queued) {
                let event = AgentEvent::Custom {
                    name: hearth_agent_proto::events::CUSTOM_STATE.to_string(),
                    value: json!({ "state": "failed", "reason": "guestd_restart" }),
                };
                let run_id = meta
                    .runs
                    .last()
                    .map(|r| r.run_id.clone())
                    .unwrap_or_else(new_ulid);
                log.append(&run_id, &event)?;
                fail_meta(&mut meta, "guestd_restart");
                self.store.write_meta(&meta)?;
            }
            self.register_cell(meta, log);
        }
        // Replay any wake-up that was acked to agentd but whose injected run did
        // not durably start before the crash (§7.2). The dedup set already holds
        // these delivery_ids, so agentd's own retry is a no-op; the inbox is the
        // recovery path for a locally-lost injection.
        for (delivery_id, task_id, text) in self.store.inbox_pending()? {
            if self.cells.lock().unwrap().contains_key(&task_id) {
                let _ = self.enqueue_run(
                    &task_id,
                    json!({ "wakeup": true, "delivery_id": delivery_id, "text": text }),
                );
            } else {
                // The task's thread is gone; the entry is unreplayable — drop it.
                let _ = self.store.inbox_ack(&delivery_id);
            }
        }
        self.rewrite_index();
        Ok(())
    }

    /// Rotate the incarnation (snapshot restore, §3.4). Every task's
    /// meta.incarnation is rewritten so outstanding cursors go `cursor.stale`.
    pub fn rotate_incarnation(&self) -> Result<()> {
        let fresh = new_ulid();
        *self.incarnation.lock().unwrap() = fresh.clone();
        let cells: Vec<Arc<TaskCell>> = self.cells.lock().unwrap().values().cloned().collect();
        for cell in cells {
            let mut meta = cell.meta.lock().unwrap();
            meta.incarnation = fresh.clone();
            self.store.write_meta(&meta)?;
        }
        Ok(())
    }

    fn thread_lock(&self, thread_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.thread_locks
            .lock()
            .unwrap()
            .entry(thread_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn register_cell(&self, meta: TaskMeta, log: crate::store::EventLog) -> Arc<TaskCell> {
        let last_seq = log.next_seq.saturating_sub(1);
        let (update_tx, _) = watch::channel((meta.state, last_seq));
        let thread_lock = self.thread_lock(&meta.thread_id);
        self.thread_index
            .lock()
            .unwrap()
            .insert(meta.thread_id.clone(), meta.task_id.clone());
        let cell = Arc::new(TaskCell {
            log: tokio::sync::Mutex::new(log),
            meta: Mutex::new(meta.clone()),
            update_tx,
            thread_lock,
            queue: Mutex::new(VecDeque::new()),
            driving: AtomicBool::new(false),
        });
        self.cells
            .lock()
            .unwrap()
            .insert(meta.task_id.clone(), cell.clone());
        cell
    }

    fn cell(&self, task_id: &str) -> Result<Arc<TaskCell>> {
        self.cells
            .lock()
            .unwrap()
            .get(task_id)
            .cloned()
            .ok_or_else(|| anyhow!("task.not_found: no task {task_id}"))
    }

    pub fn adapters(&self) -> Vec<String> {
        let mut names: Vec<String> = self.adapters.keys().cloned().collect();
        names.sort();
        names
    }

    /// Probe one adapter for its boot report (§2.1). A refusal (unpinned CLI
    /// version) surfaces as the adapter's error, loudly, exactly as §2.2 wants.
    pub async fn probe_agent(&self, name: &str) -> Result<String> {
        let adapter = self
            .adapters
            .get(name)
            .ok_or_else(|| anyhow!("no adapter {name}"))?;
        adapter.probe().await
    }

    /// Start a new task (a new thread + first run). `task_id` may be supplied
    /// by the caller (agentd, so it can ledger the grant *before* the task
    /// exists, §7.1); otherwise one is minted here.
    pub async fn start(
        self: &Arc<Self>,
        agent: &str,
        text: &str,
        initiator: Option<Initiator>,
        detach: bool,
        task_id: Option<String>,
    ) -> Result<TaskSummary> {
        if !self.adapters.contains_key(agent) {
            bail!("agent.unknown: no adapter for {agent:?}");
        }
        let task_id = task_id.unwrap_or_else(new_ulid);
        let thread_id = new_ulid();
        let incarnation = self.incarnation.lock().unwrap().clone();
        let meta = TaskMeta {
            task_id: task_id.clone(),
            thread_id: thread_id.clone(),
            agent: agent.to_string(),
            native_thread: None,
            state: TaskState::Queued,
            incarnation,
            text: text.to_string(),
            created_at: now(),
            updated_at: now(),
            result_json: None,
            pending_input_json: None,
            failure: None,
            initiator,
            runs: Vec::new(),
        };
        self.store.write_meta(&meta)?;
        let log = self.store.open_log(&task_id)?;
        self.register_cell(meta, log);
        self.enqueue_run(&task_id, json!({ "text": text }))?;
        if !detach {
            self.wait_until_settled(&task_id).await?;
        }
        self.status(&task_id)
    }

    /// Answer an `awaiting_input` task: a new run on the same thread carrying
    /// the resume payload (§3.1). Reserving the state to `queued` under the
    /// lock (before enqueuing) closes the race where two concurrent/retried
    /// responds both pass the `AwaitingInput` check and enqueue two runs — the
    /// loser now sees `queued` and is rejected.
    pub fn respond(self: &Arc<Self>, task_id: &str, response: Value) -> Result<TaskSummary> {
        let cell = self.cell(task_id)?;
        {
            let mut meta = cell.meta.lock().unwrap();
            if meta.state != TaskState::AwaitingInput {
                bail!(
                    "task.not_awaiting: task {task_id} is {} (respond only answers awaiting_input)",
                    meta.state
                );
            }
            meta.state = TaskState::Queued;
            meta.updated_at = now();
            self.store.write_meta(&meta)?;
        }
        self.enqueue_run(task_id, response)?;
        self.status(task_id)
    }

    /// Continue a completed or failed task with an ordinary user turn on its
    /// existing thread/native session. Reserving `queued` before enqueueing
    /// prevents duplicate follow-ups and keeps a new attach from observing the
    /// previous terminal state before the driver starts.
    pub fn follow_up(self: &Arc<Self>, task_id: &str, text: &str) -> Result<TaskSummary> {
        if text.trim().is_empty() {
            bail!("request.invalid: follow-up text must not be empty");
        }
        let cell = self.cell(task_id)?;
        {
            let mut meta = cell.meta.lock().unwrap();
            if !matches!(meta.state, TaskState::Completed | TaskState::Failed) {
                bail!(
                    "task.not_settled: task {task_id} is {} (follow-up requires completed or failed)",
                    meta.state
                );
            }
            meta.state = TaskState::Queued;
            meta.updated_at = now();
            meta.result_json = None;
            meta.failure = None;
            self.store.write_meta(&meta)?;
        }
        self.enqueue_run(task_id, json!({ "text": text }))?;
        self.status(task_id)
    }

    /// Deliver a wake-up as a new user turn on `thread_id` (§2.3, §7.2).
    /// Idempotent per `delivery_id`: a retried delivery is acknowledged
    /// without a second injection. Returns whether this was the first delivery.
    pub fn inject_turn(
        self: &Arc<Self>,
        delivery_id: &str,
        thread_id: &str,
        framed_text: &str,
    ) -> Result<bool> {
        let fresh = self.store.dedup_insert(delivery_id)?;
        if !fresh {
            return Ok(false);
        }
        let task_id = self
            .thread_index
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned()
            .ok_or_else(|| anyhow!("thread.not_found: no task on thread {thread_id}"))?;
        // Persist the injection durably *before* the caller can ack it: the
        // in-memory queue would otherwise lose an already-acked wake-up on a
        // guestd crash (§7.2). recover() replays any inbox entry whose run did
        // not durably start.
        self.store.inbox_put(delivery_id, &task_id, framed_text)?;
        self.enqueue_run(
            &task_id,
            json!({ "wakeup": true, "delivery_id": delivery_id, "text": framed_text }),
        )?;
        Ok(true)
    }

    pub fn cancel(&self, task_id: &str) -> Result<TaskSummary> {
        let cell = self.cell(task_id)?;
        {
            let mut meta = cell.meta.lock().unwrap();
            if meta.state.is_terminal() {
                bail!("task.terminal: task {task_id} already {}", meta.state);
            }
            meta.state = TaskState::Canceled;
            meta.updated_at = now();
            self.store.write_meta(&meta)?;
        }
        // Drop any queued turns so a pending run cannot resurrect the task; an
        // in-flight run sees the terminal state (guarded in run_one/set_terminal)
        // and stops overwriting or appending.
        cell.queue.lock().unwrap().clear();
        let last_seq = cell.update_tx.borrow().1;
        cell.update_tx.send_replace((TaskState::Canceled, last_seq));
        self.enqueue_outbox(task_id, TaskState::Canceled, None)?;
        self.rewrite_index();
        self.status(task_id)
    }

    pub fn status(&self, task_id: &str) -> Result<TaskSummary> {
        let cell = self.cell(task_id)?;
        let last_seq = *cell.update_tx.borrow();
        let meta = cell.meta.lock().unwrap();
        Ok(meta.summary(last_seq.1))
    }

    pub fn list(&self) -> Vec<TaskSummary> {
        let cells: Vec<Arc<TaskCell>> = self.cells.lock().unwrap().values().cloned().collect();
        let mut summaries: Vec<TaskSummary> = cells
            .iter()
            .map(|cell| {
                let last_seq = cell.update_tx.borrow().1;
                cell.meta.lock().unwrap().summary(last_seq)
            })
            .collect();
        summaries.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        summaries
    }

    /// Prune terminal tasks (task.gc, §3.3). Returns removed ids.
    pub fn gc(&self, keep: usize) -> Result<Vec<String>> {
        let mut terminal: Vec<TaskSummary> = self
            .list()
            .into_iter()
            .filter(|t| t.state.is_terminal())
            .collect();
        terminal.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
        let remove = terminal.len().saturating_sub(keep);
        let mut removed = Vec::new();
        for summary in terminal.into_iter().take(remove) {
            // Never remove a task whose driver is still in flight (e.g. a task
            // canceled mid-run): deleting its dir while the run holds the cell
            // would leave write_meta recreating a half-task dir over unlinked
            // segments. It becomes collectable on the next gc once quiesced.
            let driving = self
                .cells
                .lock()
                .unwrap()
                .get(&summary.task_id)
                .map(|cell| cell.driving.load(Ordering::Acquire))
                .unwrap_or(false);
            if driving {
                continue;
            }
            self.cells.lock().unwrap().remove(&summary.task_id);
            self.store.remove_task(&summary.task_id)?;
            removed.push(summary.task_id);
        }
        self.rewrite_index();
        Ok(removed)
    }

    /// Read events from a cursor (§3.4). Returns `(events, next_cursor)`.
    pub async fn events(
        &self,
        task_id: &str,
        cursor: Option<&str>,
        max: usize,
    ) -> Result<(Vec<EventRecord>, String)> {
        let cell = self.cell(task_id)?;
        let from = {
            let meta = cell.meta.lock().unwrap();
            resolve_cursor(&meta, cursor)?
        };
        let log = cell.log.lock().await;
        let records = log.read_from(from, max)?;
        let next_seq = records
            .last()
            .map(|r| r.seq)
            .unwrap_or(from.saturating_sub(1));
        let cursor = {
            let meta = cell.meta.lock().unwrap();
            make_cursor(&meta, next_seq)
        };
        Ok((records, cursor))
    }

    /// Subscribe to a task's `(state, last_seq)` updates for long-poll/attach.
    pub fn subscribe(&self, task_id: &str) -> Result<watch::Receiver<(TaskState, u64)>> {
        Ok(self.cell(task_id)?.update_tx.subscribe())
    }

    /// Block until the task leaves `queued`/`running` (settled at terminal or
    /// awaiting_input) — used by non-detached start.
    pub async fn wait_until_settled(&self, task_id: &str) -> Result<()> {
        let mut rx = self.subscribe(task_id)?;
        loop {
            let (state, _) = *rx.borrow_and_update();
            if state.is_terminal() || state == TaskState::AwaitingInput {
                return Ok(());
            }
            if rx.changed().await.is_err() {
                return Ok(());
            }
        }
    }

    fn rewrite_index(&self) {
        if let Err(err) = self.store.rewrite_index(&self.list()) {
            tracing::warn!(error = %err, "failed to rewrite tasks.index");
        }
    }

    fn enqueue_outbox(
        &self,
        task_id: &str,
        transition: TaskState,
        detail: Option<Value>,
    ) -> Result<()> {
        let delivery = Delivery {
            delivery_id: new_ulid(),
            task_id: task_id.to_string(),
            transition,
            detail,
            created: now(),
        };
        self.store.outbox_put(&delivery)?;
        self.outbox_notify.notify_waiters();
        Ok(())
    }

    /// Push an input onto the task's turn queue and ensure a driver is running.
    fn enqueue_run(self: &Arc<Self>, task_id: &str, input: Value) -> Result<()> {
        let cell = self.cell(task_id)?;
        cell.queue.lock().unwrap().push_back(input);
        if cell
            .driving
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let engine = Arc::clone(self);
            let task_id = task_id.to_string();
            tokio::spawn(async move {
                engine.drive(&task_id).await;
            });
        }
        Ok(())
    }

    /// Drain a task's turn queue, running each turn under the thread lock.
    async fn drive(self: &Arc<Self>, task_id: &str) {
        let Ok(cell) = self.cell(task_id) else {
            return;
        };
        let _thread = cell.thread_lock.clone().lock_owned().await;
        loop {
            let input = cell.queue.lock().unwrap().pop_front();
            let Some(input) = input else {
                cell.driving.store(false, Ordering::Release);
                // A turn enqueued between pop and store must not be stranded.
                if cell.queue.lock().unwrap().is_empty() {
                    return;
                }
                if cell
                    .driving
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return;
                }
                continue;
            };
            if let Err(err) = self.run_one(&cell, task_id, input).await {
                tracing::error!(task = %task_id, error = %err, "run failed");
                // Finalize failed exactly like the adapter-error path: publish
                // the terminal state and enqueue the durable Failed wake-up, so
                // a non-detached start / attach unblocks and the initiator is
                // still woken (§7.2). A bare fail_meta would leave followers
                // hung on a stale `running` watch value.
                let already_terminal = cell.meta.lock().unwrap().state.is_terminal();
                if !already_terminal {
                    let _ = self
                        .set_terminal(
                            &cell,
                            task_id,
                            TaskState::Failed,
                            Some(format!("engine_error: {err}")),
                            None,
                        )
                        .await;
                }
            }
        }
    }

    async fn run_one(
        self: &Arc<Self>,
        cell: &Arc<TaskCell>,
        task_id: &str,
        input: Value,
    ) -> Result<()> {
        let run_id = new_ulid();
        let (agent, native_thread, thread_id) = {
            let mut meta = cell.meta.lock().unwrap();
            // A cancel is a hard stop: never resurrect a canceled task, even if
            // a turn was popped just before `cancel` cleared the queue. Other
            // terminal states (completed/failed) may legitimately re-open here —
            // that is exactly how a wake-up enters a thread whose prior task
            // already finished (§2.3, "a thread may serve successive tasks").
            if meta.state == TaskState::Canceled {
                return Ok(());
            }
            meta.state = TaskState::Running;
            meta.updated_at = now();
            meta.runs.push(RunRecord {
                run_id: run_id.clone(),
                outcome: None,
                started_at: now(),
                ended_at: None,
            });
            self.store.write_meta(&meta)?;
            (
                meta.agent.clone(),
                meta.native_thread.clone(),
                meta.thread_id.clone(),
            )
        };
        self.append(
            cell,
            &run_id,
            AgentEvent::RunStarted {
                thread_id: thread_id.clone(),
                run_id: run_id.clone(),
            },
        )
        .await?;
        self.publish(cell, TaskState::Running).await;
        // The run is now durable in the event log: a wake-up's inbox entry can
        // be released. (A crash before this point leaves the entry for recover.)
        if let Some(delivery_id) = input.get("delivery_id").and_then(Value::as_str) {
            let _ = self.store.inbox_ack(delivery_id);
        }

        let adapter = self.adapters.get(&agent).cloned();
        let Some(adapter) = adapter else {
            bail!("agent.unknown: adapter {agent} vanished");
        };
        // Wake-ups carry provenance-framed text; other inputs are task text or
        // a resume payload the adapter interprets natively.
        let adapter_input = input.clone();
        let output = adapter
            .run(&thread_id, native_thread.as_deref(), &adapter_input)
            .await;
        let output = match output {
            Ok(output) => output,
            Err(err) => {
                self.append(
                    cell,
                    &run_id,
                    AgentEvent::RunError {
                        message: err.to_string(),
                        code: Some("adapter_error".to_string()),
                    },
                )
                .await?;
                self.finish_run(cell, &run_id, RunOutcome::Error);
                self.set_terminal(
                    cell,
                    task_id,
                    TaskState::Failed,
                    Some(err.to_string()),
                    None,
                )
                .await?;
                return Ok(());
            }
        };
        if let Some(native) = output.native_thread {
            let mut meta = cell.meta.lock().unwrap();
            meta.native_thread = Some(native);
            self.store.write_meta(&meta)?;
        }
        self.apply_adapter_events(cell, task_id, &run_id, output.events)
            .await
    }

    async fn apply_adapter_events(
        self: &Arc<Self>,
        cell: &Arc<TaskCell>,
        task_id: &str,
        run_id: &str,
        events: Vec<AdapterEvent>,
    ) -> Result<()> {
        // Capture the thread id once; never hold the meta guard across an await.
        let thread_id = cell.meta.lock().unwrap().thread_id.clone();
        for event in events {
            // A cancel that landed mid-run finalized the task; stop appending
            // and never overwrite the terminal state (§3.2, the cancel path).
            if cell.meta.lock().unwrap().state.is_terminal() {
                return Ok(());
            }
            match event {
                AdapterEvent::Event(event) => {
                    self.append(cell, run_id, event).await?;
                }
                AdapterEvent::AwaitingInput { prompt } => {
                    self.append(cell, run_id, AgentEvent::permission_request(&prompt))
                        .await?;
                    self.append(
                        cell,
                        run_id,
                        AgentEvent::RunFinished {
                            thread_id: thread_id.clone(),
                            run_id: run_id.to_string(),
                            result: None,
                        },
                    )
                    .await?;
                    self.finish_run(cell, run_id, RunOutcome::Interrupted);
                    {
                        let mut meta = cell.meta.lock().unwrap();
                        meta.state = TaskState::AwaitingInput;
                        meta.pending_input_json = Some(serde_json::to_string(&prompt)?);
                        meta.updated_at = now();
                        self.store.write_meta(&meta)?;
                    }
                    self.append(
                        cell,
                        run_id,
                        AgentEvent::state_change(&TaskState::AwaitingInput, None),
                    )
                    .await?;
                    self.publish(cell, TaskState::AwaitingInput).await;
                    self.enqueue_outbox(task_id, TaskState::AwaitingInput, Some(prompt))?;
                    return Ok(());
                }
                AdapterEvent::Finished { result } => {
                    self.append(
                        cell,
                        run_id,
                        AgentEvent::RunFinished {
                            thread_id: thread_id.clone(),
                            run_id: run_id.to_string(),
                            result: Some(result.clone()),
                        },
                    )
                    .await?;
                    self.finish_run(cell, run_id, RunOutcome::Finished);
                    self.set_terminal(cell, task_id, TaskState::Completed, None, Some(result))
                        .await?;
                    return Ok(());
                }
                AdapterEvent::Failed { message } => {
                    self.append(
                        cell,
                        run_id,
                        AgentEvent::RunError {
                            message: message.clone(),
                            code: None,
                        },
                    )
                    .await?;
                    self.finish_run(cell, run_id, RunOutcome::Error);
                    self.set_terminal(cell, task_id, TaskState::Failed, Some(message), None)
                        .await?;
                    return Ok(());
                }
            }
        }
        // The adapter returned without a terminal/awaiting event: treat the run
        // as finished with no result rather than hang the task forever.
        self.append(
            cell,
            run_id,
            AgentEvent::RunFinished {
                thread_id,
                run_id: run_id.to_string(),
                result: None,
            },
        )
        .await?;
        self.finish_run(cell, run_id, RunOutcome::Finished);
        self.set_terminal(cell, task_id, TaskState::Completed, None, Some(json!({})))
            .await
    }

    fn finish_run(&self, cell: &Arc<TaskCell>, run_id: &str, outcome: RunOutcome) {
        let mut meta = cell.meta.lock().unwrap();
        if let Some(run) = meta.runs.iter_mut().find(|r| r.run_id == run_id) {
            run.outcome = Some(outcome);
            run.ended_at = Some(now());
        }
        let _ = self.store.write_meta(&meta);
    }

    async fn set_terminal(
        self: &Arc<Self>,
        cell: &Arc<TaskCell>,
        task_id: &str,
        state: TaskState,
        failure: Option<String>,
        result: Option<Value>,
    ) -> Result<()> {
        {
            let mut meta = cell.meta.lock().unwrap();
            // Already finalized (a cancel raced this run to completion): keep
            // the recorded terminal state and do not emit a second, conflicting
            // outcome + outbox delivery.
            if meta.state.is_terminal() {
                return Ok(());
            }
            meta.state = state;
            meta.failure = failure;
            meta.pending_input_json = None;
            meta.result_json = match &result {
                Some(value) => Some(serde_json::to_string(value)?),
                None => None,
            };
            meta.updated_at = now();
            self.store.write_meta(&meta)?;
        }
        self.append(cell, "", AgentEvent::state_change(&state, None))
            .await?;
        self.publish(cell, state).await;
        self.enqueue_outbox(task_id, state, result)?;
        self.rewrite_index();
        Ok(())
    }

    async fn append(&self, cell: &Arc<TaskCell>, run_id: &str, event: AgentEvent) -> Result<()> {
        let seq = {
            let mut log = cell.log.lock().await;
            log.append(run_id, &event)?
        };
        let state = cell.meta.lock().unwrap().state;
        cell.update_tx.send_replace((state, seq));
        Ok(())
    }

    async fn publish(&self, cell: &Arc<TaskCell>, state: TaskState) {
        let seq = cell.update_tx.borrow().1;
        cell.update_tx.send_replace((state, seq));
    }
}
