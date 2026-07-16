//! Phase 2 acceptance (docs/agent-plane.md §11–12): the task registry and the
//! codex vertical, driven directly against a guestd's task-verb server. Start,
//! tail, answer an approval via interrupt→new-run, cancel; durability across a
//! fresh registry open; the cursor/incarnation contract.

use hearth_e2e::{guest_verb, AgentSpec, Harness, HarnessOptions};
use hearth_agent_proto::AgentVerb;
use serde_json::{json, Map, Value};
use std::time::Duration;

fn opts() -> HarnessOptions {
    HarnessOptions {
        agents: vec![AgentSpec::worker("worker")],
        delegators: vec![],
        http: None,
        codex_command: env!("CARGO_BIN_EXE_fake_codex").to_string(),
        claude_command: None,
    }
}

async fn start_task(h: &Harness, text: &str) -> (tokio::net::UnixStream, String) {
    let mut stream = h.guest_connect("worker").await.unwrap();
    let mut args = Map::new();
    args.insert("agent".to_string(), json!("codex"));
    args.insert("text".to_string(), json!(text));
    args.insert("detach".to_string(), json!(false));
    let summary = guest_verb(&mut stream, AgentVerb::TaskStart, args).await.unwrap();
    let task_id = summary["task_id"].as_str().unwrap().to_string();
    (stream, task_id)
}

async fn task_status(stream: &mut tokio::net::UnixStream, task_id: &str) -> Value {
    let mut args = Map::new();
    args.insert("task_id".to_string(), json!(task_id));
    guest_verb(stream, AgentVerb::TaskStatus, args).await.unwrap()
}

#[tokio::test]
async fn start_streams_events_and_completes() {
    let h = Harness::start(opts()).await.unwrap();
    let (mut stream, task_id) = start_task(&h, "hello world").await;
    let status = task_status(&mut stream, &task_id).await;
    assert_eq!(status["state"], json!("completed"));

    // The event log holds the AG-UI vocabulary end-to-end: run lifecycle, text,
    // tool call — one schema, no translation after the adapter (trace (a)/(b)).
    let mut args = Map::new();
    args.insert("task_id".to_string(), json!(task_id));
    args.insert("max".to_string(), json!(100));
    let events = guest_verb(&mut stream, AgentVerb::TaskEvents, args).await.unwrap();
    let types: Vec<String> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["event"]["type"].as_str().map(str::to_string))
        .collect();
    assert!(types.contains(&"RUN_STARTED".to_string()));
    assert!(types.contains(&"TEXT_MESSAGE_CONTENT".to_string()));
    assert!(types.contains(&"TOOL_CALL_START".to_string()));
    assert!(types.contains(&"TOOL_CALL_RESULT".to_string()));
    assert!(types.contains(&"RUN_FINISHED".to_string()));
}

#[tokio::test]
async fn approval_interrupts_then_a_new_run_resumes_the_thread() {
    let h = Harness::start(opts()).await.unwrap();
    // The task text scripts the fake codex to raise an exec approval.
    let (mut stream, task_id) = start_task(&h, "please NEEDS_APPROVAL run it").await;
    let status = task_status(&mut stream, &task_id).await;
    assert_eq!(
        status["state"],
        json!("awaiting_input"),
        "an approval request ends the run interrupted and the task awaits input"
    );
    assert!(status["pending_input"]["kind"] == json!("exec_approval"));

    // Answering starts a NEW run on the same thread (§3.1) that completes.
    let mut args = Map::new();
    args.insert("task_id".to_string(), json!(task_id));
    args.insert(
        "response".to_string(),
        json!({ "approval": { "decision": "allow" } }),
    );
    guest_verb(&mut stream, AgentVerb::TaskRespond, args).await.unwrap();
    // Give the new run a beat to finish.
    let settled = wait_state(&h, "worker", &task_id, "completed").await;
    assert!(settled, "resumed run should complete the task");

    // Two runs recorded: the first interrupted, the second finished.
    let status = task_status(&mut stream, &task_id).await;
    let runs = status["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["outcome"], json!("interrupted"));
    assert_eq!(runs[1]["outcome"], json!("finished"));
}

#[tokio::test]
async fn cancel_moves_a_task_to_canceled() {
    let h = Harness::start(opts()).await.unwrap();
    // Interrupt so the task rests in awaiting_input, then cancel it.
    let (mut stream, task_id) = start_task(&h, "NEEDS_APPROVAL").await;
    let status = task_status(&mut stream, &task_id).await;
    assert_eq!(status["state"], json!("awaiting_input"));
    let mut args = Map::new();
    args.insert("task_id".to_string(), json!(task_id));
    let canceled = guest_verb(&mut stream, AgentVerb::TaskCancel, args).await.unwrap();
    assert_eq!(canceled["state"], json!("canceled"));
}

#[tokio::test]
async fn stale_cursor_is_rejected_by_incarnation() {
    let h = Harness::start(opts()).await.unwrap();
    let (mut stream, task_id) = start_task(&h, "hello").await;
    let status = task_status(&mut stream, &task_id).await;
    let incarnation = status["incarnation"].as_str().unwrap();
    // A cursor from a *different* incarnation stales (the snapshot-restore
    // contract, §3.4) — the guest never silently replays the wrong events.
    let mut args = Map::new();
    args.insert("task_id".to_string(), json!(task_id));
    args.insert("cursor".to_string(), json!("BOGUSINCARNATION.1"));
    let err = guest_verb(&mut stream, AgentVerb::TaskEvents, args).await.unwrap_err();
    assert!(err.to_string().contains("cursor.stale"), "got: {err}");
    // The real incarnation still resolves.
    let mut args = Map::new();
    args.insert("task_id".to_string(), json!(task_id));
    args.insert("cursor".to_string(), json!(format!("{incarnation}.0")));
    assert!(guest_verb(&mut stream, AgentVerb::TaskEvents, args).await.is_ok());
}

/// Poll a guest task until it reaches `want` or a short timeout.
async fn wait_state(h: &Harness, vm: &str, task_id: &str, want: &str) -> bool {
    for _ in 0..200 {
        let mut stream = h.guest_connect(vm).await.unwrap();
        let mut args = Map::new();
        args.insert("task_id".to_string(), json!(task_id));
        let status = guest_verb(&mut stream, AgentVerb::TaskStatus, args).await.unwrap();
        if status["state"] == json!(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

// Regression: a canceled task must stay canceled — a run that was in flight or
// queued when cancel landed must not resurrect it back through running to
// completed (review finding: cancel didn't stop in-flight/queued work).
#[tokio::test]
async fn cancel_is_terminal_even_against_an_in_flight_run() {
    let h = Harness::start(opts()).await.unwrap();
    // Start detached so the run drives concurrently, and cancel immediately —
    // racing the driver. Repeat to hit both the queued and in-flight windows.
    for _ in 0..10 {
        let mut stream = h.guest_connect("worker").await.unwrap();
        let mut args = Map::new();
        args.insert("agent".to_string(), json!("codex"));
        args.insert("text".to_string(), json!("a normal task that completes"));
        args.insert("detach".to_string(), json!(true));
        let summary = guest_verb(&mut stream, AgentVerb::TaskStart, args).await.unwrap();
        let task_id = summary["task_id"].as_str().unwrap().to_string();

        let mut args = Map::new();
        args.insert("task_id".to_string(), json!(task_id));
        let canceled = guest_verb(&mut stream, AgentVerb::TaskCancel, args).await.unwrap();
        assert_eq!(canceled["state"], json!("canceled"));

        // Give any stray run a chance to (wrongly) overwrite the terminal state.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let final_state = task_status(&mut stream, &task_id).await["state"].clone();
        assert_eq!(
            final_state,
            json!("canceled"),
            "a canceled task must never be resurrected to {final_state}"
        );
    }
}

// Regression: two concurrent/retried responds to one awaiting_input task must
// not both start a run (review finding: respond had a check-then-enqueue race).
#[tokio::test]
async fn a_second_respond_is_rejected_no_duplicate_run() {
    let h = Harness::start(opts()).await.unwrap();
    let (mut stream, task_id) = start_task(&h, "NEEDS_APPROVAL").await;
    assert_eq!(task_status(&mut stream, &task_id).await["state"], json!("awaiting_input"));

    let respond = |resp: &str| {
        let mut a = Map::new();
        a.insert("task_id".to_string(), json!(task_id));
        a.insert("response".to_string(), json!({ "approval": { "decision": resp } }));
        a
    };
    // First respond succeeds; the reservation flips state off awaiting_input.
    let mut s1 = h.guest_connect("worker").await.unwrap();
    guest_verb(&mut s1, AgentVerb::TaskRespond, respond("allow")).await.unwrap();
    // A second respond now sees a non-awaiting state and is refused.
    let mut s2 = h.guest_connect("worker").await.unwrap();
    let err = guest_verb(&mut s2, AgentVerb::TaskRespond, respond("allow")).await.unwrap_err();
    assert!(err.to_string().contains("task.not_awaiting"), "got: {err}");

    // Exactly one resume ran: the interrupted run plus one finished run = 2.
    assert!(wait_state(&h, "worker", &task_id, "completed").await);
    let runs = task_status(&mut stream, &task_id).await["runs"].as_array().unwrap().len();
    assert_eq!(runs, 2, "a rejected second respond must not add a third run");
}
