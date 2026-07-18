//! Phase 6 acceptance: Hermes-only guest discovery, structured ACP message and
//! tool events, native-session continuation, and ACP permission bridging.

use hearth_agent_proto::AgentVerb;
use hearth_e2e::{guest_verb, AgentSpec, Harness, HarnessOptions};
use serde_json::{json, Map, Value};
use std::time::Duration;
use ulid::Ulid;

fn opts() -> HarnessOptions {
    HarnessOptions {
        agents: vec![AgentSpec::worker("worker")],
        delegators: vec![],
        http: None,
        codex_command: None,
        claude_command: None,
        hermes_command: Some(env!("CARGO_BIN_EXE_fake_hermes").to_string()),
    }
}

#[tokio::test]
async fn hermes_only_is_selected_and_its_session_resumes() {
    let h = Harness::start(opts()).await.unwrap();

    let listed = h.agent(AgentVerb::AgentLs, Map::new()).await.unwrap();
    assert_eq!(listed["agents"][0]["adapters"], json!(["hermes"]));

    // The operator names the VM, not an adapter. agentd must select the healthy
    // Hermes declaration instead of falling back to its old codex default.
    let mut args = Map::new();
    args.insert("agent".to_string(), json!("worker"));
    args.insert("text".to_string(), json!("hello hermes"));
    let started = h.agent(AgentVerb::TaskStart, args).await.unwrap();
    let task_id = started["task_id"].as_str().unwrap().to_string();
    let status = wait_for_state(&h, &task_id, "completed", 1).await;
    assert_eq!(status["agent"], json!("hermes"));
    assert_eq!(status["state"], json!("completed"));
    assert_eq!(
        status["result"]["summary"],
        json!("hermes echo: hello hermes")
    );
    let thread_id = status["thread_id"].as_str().unwrap().to_string();

    // A wake-up is a new turn on the same Hearth thread. The adapter passes
    // the persisted Hermes ACP session id through session/load.
    let mut guest = h.guest_connect("worker").await.unwrap();
    let mut args = Map::new();
    args.insert("delivery_id".to_string(), json!(Ulid::new().to_string()));
    args.insert("thread_id".to_string(), json!(thread_id));
    args.insert("text".to_string(), json!("follow-up"));
    guest_verb(&mut guest, AgentVerb::InjectTurn, args)
        .await
        .unwrap();

    let status = wait_for_runs(&h, &task_id, 2).await;
    assert_eq!(status["state"], json!("completed"));
    assert_eq!(
        status["result"]["summary"],
        json!("hermes resumed: follow-up")
    );
}

#[tokio::test]
async fn hermes_thought_is_durable_before_the_prompt_finishes() {
    let h = Harness::start(opts()).await.unwrap();

    let mut args = Map::new();
    args.insert("agent".to_string(), json!("worker"));
    args.insert("text".to_string(), json!("slow thought"));
    let started = h.agent(AgentVerb::TaskStart, args).await.unwrap();
    let task_id = started["task_id"].as_str().unwrap().to_string();

    let mut guest = h.guest_connect("worker").await.unwrap();
    let mut observed_live_thought = false;
    for _ in 0..100 {
        let mut args = Map::new();
        args.insert("task_id".to_string(), json!(task_id));
        args.insert("max".to_string(), json!(100));
        let events = guest_verb(&mut guest, AgentVerb::TaskEvents, args)
            .await
            .unwrap();
        let records = events["events"].as_array().unwrap();
        if records.iter().any(|record| {
            record["event"]["type"] == json!("REASONING_MESSAGE_CONTENT")
                && record["event"]["delta"] == json!("considering the request")
        }) {
            assert!(
                records
                    .iter()
                    .all(|record| record["event"]["type"] != json!("RUN_FINISHED")),
                "the thought must be observable before the terminal event"
            );
            observed_live_thought = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        observed_live_thought,
        "Hermes thought chunk was buffered until run completion"
    );

    let status = wait_for_state(&h, &task_id, "completed", 1).await;
    assert_eq!(status["result"]["summary"], json!("finished thinking"));
}

#[tokio::test]
async fn hermes_acp_permission_interrupts_and_responds_on_the_live_prompt() {
    let h = Harness::start(opts()).await.unwrap();

    let mut args = Map::new();
    args.insert("agent".to_string(), json!("worker"));
    args.insert("text".to_string(), json!("needs approval"));
    let started = h.agent(AgentVerb::TaskStart, args).await.unwrap();
    let task_ref = started["task_ref"].as_str().unwrap();
    let task_id = started["task_id"].as_str().unwrap();
    let status = wait_for_state(&h, task_id, "awaiting_input", 1).await;
    assert_eq!(status["state"], json!("awaiting_input"));
    assert_eq!(status["pending_input"]["protocol"], json!("acp"));
    assert_eq!(status["pending_input"]["kind"], json!("permission"));

    let mut args = Map::new();
    args.insert("task_ref".to_string(), json!(task_ref));
    args.insert("response".to_string(), json!({ "text": "allow once" }));
    h.agent(AgentVerb::TaskRespond, args).await.unwrap();

    let status = wait_for_state(&h, task_id, "completed", 2).await;
    assert_eq!(status["state"], json!("completed"));
    assert_eq!(
        status["result"]["summary"],
        json!("hermes approved: allow_once")
    );
    assert_eq!(status["runs"].as_array().unwrap().len(), 2);
}

async fn wait_for_runs(h: &Harness, task_id: &str, count: usize) -> Value {
    wait_for_state(h, task_id, "completed", count).await
}

async fn wait_for_state(h: &Harness, task_id: &str, state: &str, count: usize) -> Value {
    for _ in 0..200 {
        let mut guest = h.guest_connect("worker").await.unwrap();
        let mut args = Map::new();
        args.insert("task_id".to_string(), json!(task_id));
        let status = guest_verb(&mut guest, AgentVerb::TaskStatus, args)
            .await
            .unwrap();
        if status["runs"]
            .as_array()
            .is_some_and(|runs| runs.len() == count)
            && status["state"] == json!(state)
        {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("Hermes task did not reach {state} with {count} run(s)");
}
