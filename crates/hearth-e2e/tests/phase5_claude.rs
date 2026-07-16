//! Phase 5 acceptance (docs/agent-plane.md §11): the second adapter (claude),
//! proven against the same task engine and traces that codex passes. Same
//! interrupt→resume lifecycle, driven through claude's stream-json contract.

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
        claude_command: Some(env!("CARGO_BIN_EXE_fake_claude").to_string()),
    }
}

async fn start_claude(h: &Harness, text: &str) -> (tokio::net::UnixStream, String) {
    let mut stream = h.guest_connect("worker").await.unwrap();
    let mut args = Map::new();
    args.insert("agent".to_string(), json!("claude"));
    args.insert("text".to_string(), json!(text));
    args.insert("detach".to_string(), json!(false));
    let summary = guest_verb(&mut stream, AgentVerb::TaskStart, args).await.unwrap();
    (stream, summary["task_id"].as_str().unwrap().to_string())
}

async fn task_status(stream: &mut tokio::net::UnixStream, task_id: &str) -> Value {
    let mut args = Map::new();
    args.insert("task_id".to_string(), json!(task_id));
    guest_verb(stream, AgentVerb::TaskStatus, args).await.unwrap()
}

#[tokio::test]
async fn claude_adapter_is_advertised() {
    let h = Harness::start(opts()).await.unwrap();
    let mut stream = h.guest_connect("worker").await.unwrap();
    let agents = guest_verb(&mut stream, AgentVerb::AgentLs, Map::new()).await.unwrap();
    let names: Vec<&str> = agents["agents"].as_array().unwrap().iter().filter_map(|a| a.as_str()).collect();
    assert!(names.contains(&"claude"), "claude adapter registered: {names:?}");
    assert!(names.contains(&"codex"), "codex still registered too");
}

#[tokio::test]
async fn claude_task_streams_and_completes() {
    let h = Harness::start(opts()).await.unwrap();
    let (mut stream, task_id) = start_claude(&h, "hello claude").await;
    let status = task_status(&mut stream, &task_id).await;
    assert_eq!(status["state"], json!("completed"));

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
    assert!(types.contains(&"TEXT_MESSAGE_CONTENT".to_string()));
    assert!(types.contains(&"TOOL_CALL_START".to_string()));
    assert!(types.contains(&"RUN_FINISHED".to_string()));
}

#[tokio::test]
async fn claude_permission_prompt_interrupts_then_resumes() {
    let h = Harness::start(opts()).await.unwrap();
    let (mut stream, task_id) = start_claude(&h, "NEEDS_APPROVAL delete stuff").await;
    let status = task_status(&mut stream, &task_id).await;
    assert_eq!(
        status["state"],
        json!("awaiting_input"),
        "claude's permission prompt maps to awaiting_input"
    );

    let mut args = Map::new();
    args.insert("task_id".to_string(), json!(task_id));
    args.insert("response".to_string(), json!({ "text": "allow" }));
    guest_verb(&mut stream, AgentVerb::TaskRespond, args).await.unwrap();

    // The resumed run (a `--resume` invocation) completes the task.
    for _ in 0..200 {
        if task_status(&mut stream, &task_id).await["state"] == json!("completed") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("claude resume did not complete the task");
}
