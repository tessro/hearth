//! Phase 4 acceptance (docs/agent-plane.md §11–12, trace (c)/(e)): agent-to-
//! agent delegation with durable wake-ups. The agent-in-charge delegates, the
//! callee hits `awaiting_input` **while agentd is stopped**, agentd restarts,
//! the initiator is woken **exactly once**, responds, and collects the result —
//! with a full ledger + audit trail. A non-allowlisted VM's `delegate` is
//! rejected and ledgered.

use hearth_agent_proto::events::CUSTOM_SESSION_NAME;
use hearth_agent_proto::AgentVerb;
use hearth_e2e::{guest_verb, AgentSpec, Harness, HarnessOptions, McpClient};
use serde_json::{json, Map, Value};
use std::time::Duration;

fn opts() -> HarnessOptions {
    HarnessOptions {
        agents: vec![AgentSpec::boss("boss"), AgentSpec::worker("worker")],
        delegators: vec!["boss".to_string()],
        http: None,
        codex_command: Some(env!("CARGO_BIN_EXE_fake_codex").to_string()),
        claude_command: None,
        hermes_command: None,
    }
}

/// Start a task on `vm` and return `(task_id, thread_id)`.
async fn start_task(h: &Harness, vm: &str, text: &str) -> (String, String) {
    let mut stream = h.guest_connect(vm).await.unwrap();
    let mut args = Map::new();
    args.insert("agent".to_string(), json!("codex"));
    args.insert("text".to_string(), json!(text));
    args.insert("detach".to_string(), json!(false));
    let summary = guest_verb(&mut stream, AgentVerb::TaskStart, args)
        .await
        .unwrap();
    (
        summary["task_id"].as_str().unwrap().to_string(),
        summary["thread_id"].as_str().unwrap().to_string(),
    )
}

async fn task_status(h: &Harness, vm: &str, task_id: &str) -> Value {
    let mut stream = h.guest_connect(vm).await.unwrap();
    let mut args = Map::new();
    args.insert("task_id".to_string(), json!(task_id));
    guest_verb(&mut stream, AgentVerb::TaskStatus, args)
        .await
        .unwrap()
}

/// Poll a task on `vm` until it reaches `want` (or times out).
async fn wait_state(h: &Harness, vm: &str, task_id: &str, want: &str) -> bool {
    for _ in 0..400 {
        if task_status(h, vm, task_id).await["state"] == json!(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Poll a task on `vm` until it has at least `want` runs.
async fn wait_runs(h: &Harness, vm: &str, task_id: &str, want: usize) -> bool {
    for _ in 0..400 {
        let runs = task_status(h, vm, task_id).await["runs"]
            .as_array()
            .map(|r| r.len())
            .unwrap_or(0);
        if runs >= want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test]
async fn calling_agent_can_replace_its_durable_session_name_over_mcp() {
    let h = Harness::start(opts()).await.unwrap();
    let (task_id, thread_id) = start_task(&h, "boss", "Investigate the slow checkout").await;
    let mut mcp = McpClient::connect(&h, "boss", &thread_id).await.unwrap();

    let renamed = mcp
        .call_tool(
            "set_session_name",
            json!({ "name": "Trace checkout latency" }),
        )
        .await
        .unwrap();
    assert_eq!(renamed["name"], json!("Trace checkout latency"));

    let status = task_status(&h, "boss", &task_id).await;
    assert_eq!(status["session_name"], json!("Trace checkout latency"));

    let mut guest = h.guest_connect("boss").await.unwrap();
    let mut args = Map::new();
    args.insert("task_id".to_string(), json!(task_id));
    let replay = guest_verb(&mut guest, AgentVerb::TaskEvents, args)
        .await
        .unwrap();
    assert!(replay["events"].as_array().unwrap().iter().any(|record| {
        record["event"]["type"] == json!("CUSTOM")
            && record["event"]["name"] == json!(CUSTOM_SESSION_NAME)
            && record["event"]["value"]["name"] == json!("Trace checkout latency")
    }));
}

#[tokio::test]
async fn delegation_survives_agentd_restart_and_wakes_the_initiator_exactly_once() {
    let h = Harness::start(opts()).await.unwrap();

    // The boss has a live thread to be woken on.
    let (boss_task, boss_thread) = start_task(&h, "boss", "boss idle").await;
    assert_eq!(
        task_status(&h, "boss", &boss_task).await["state"],
        json!("completed")
    );

    // The boss delegates a task that will need approval — via MCP, occupying
    // the user seat of the callee's task (§6). Crucially, NO explicit
    // initiator_thread is passed: the wake target must come from the shim's
    // hello thread_id (the calling agent's own session), the production path.
    let mut boss_mcp = McpClient::connect(&h, "boss", &boss_thread).await.unwrap();
    let delegated = boss_mcp
        .call_tool(
            "delegate",
            json!({
                "agent": "worker",
                "task": "NEEDS_APPROVAL run the migration",
            }),
        )
        .await
        .unwrap();
    let worker_ref = delegated["task_ref"].as_str().unwrap().to_string();
    let worker_task = delegated["task_id"].as_str().unwrap().to_string();

    // Crash agentd while the callee is (about to be) awaiting_input.
    h.stop_agentd().await;

    // The worker reaches awaiting_input on its own; its outbox now holds a
    // pending wake-up that cannot be delivered (agentd is down).
    assert!(
        wait_state(&h, "worker", &worker_task, "awaiting_input").await,
        "worker should reach awaiting_input even with agentd down"
    );

    // Restart agentd: it re-brokers listeners, reloads the ledger, and the
    // worker replays its outbox → the boss is woken.
    h.start_agentd().await.unwrap();

    // The boss thread gets exactly one new run (the injected wake-up), proving
    // at-least-once delivery + idempotent injection = woken exactly once.
    assert!(
        wait_runs(&h, "boss", &boss_task, 2).await,
        "the boss should be woken with a fresh run"
    );
    // Give any duplicate deliveries a chance to (wrongly) inject, then assert
    // exactly one wake-up run landed.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let boss_runs = task_status(&h, "boss", &boss_task).await["runs"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(boss_runs, 2, "woken exactly once (dedup by delivery_id)");

    // The boss answers the callee (a fresh MCP session, post-restart).
    let mut boss_mcp = McpClient::connect(&h, "boss", &boss_thread).await.unwrap();
    boss_mcp
        .call_tool(
            "task_respond",
            json!({ "task_ref": worker_ref, "response": { "approval": { "decision": "allow" } } }),
        )
        .await
        .unwrap();

    // The callee resumes on the new run. `wait_for` is the streaming workhorse
    // — it returns on any change, so drive it until the task settles terminal
    // (a real caller loops the same way, spending context deliberately).
    let mut settled = false;
    for _ in 0..40 {
        let waited = boss_mcp
            .call_tool(
                "wait_for",
                json!({ "task_ref": worker_ref, "timeout_seconds": 5 }),
            )
            .await
            .unwrap();
        if waited["state"] == json!("completed") {
            settled = true;
            break;
        }
    }
    assert!(settled, "callee completed after the answer");
    assert!(
        wait_state(&h, "worker", &worker_task, "completed").await,
        "worker task reaches completed"
    );

    // The ledger recorded the grant (wake-up authority, §4.4).
    let ledger = std::fs::read_to_string(h.root.join("ledger").join("delegations.log")).unwrap();
    assert!(
        ledger.contains("\"granted\""),
        "delegation grant is ledgered"
    );
    assert!(
        ledger.contains(&worker_task),
        "the callee task is in the ledger"
    );
}

#[tokio::test]
async fn a_non_allowlisted_delegation_is_rejected_and_ledgered() {
    let h = Harness::start(opts()).await.unwrap();
    // The worker is NOT in the delegators allowlist; its delegate is refused.
    let (_wtask, wthread) = start_task(&h, "worker", "worker idle").await;
    let mut worker_mcp = McpClient::connect(&h, "worker", &wthread).await.unwrap();
    let err = worker_mcp
        .call_tool(
            "delegate",
            json!({ "agent": "boss", "task": "do something", "initiator_thread": wthread }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("rejected"), "got: {err}");

    // The rejection is ledgered (the A2A `rejected` state lives here, §3.2).
    let ledger = std::fs::read_to_string(h.root.join("ledger").join("delegations.log")).unwrap();
    assert!(
        ledger.contains("\"rejected\""),
        "rejection is ledgered: {ledger}"
    );
}
