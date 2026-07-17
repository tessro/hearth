//! Phase 3 acceptance (docs/agent-plane.md §11): the AG-UI HTTP leg. An AG-UI
//! client drives task → interrupt → resume; SSE detach/reattach replays
//! losslessly; two UIs watch one task; auth is required end-to-end.
//!
//! The client here is a raw HTTP/SSE client that mirrors what an unmodified
//! AG-UI `HttpAgent` does on the wire (POST `RunAgentInput`, parse `data:`
//! `BaseEvent`s). Conformance against the real TS `HttpAgent` is noted in
//! docs/agent-plane-verification.md.

use hearth_e2e::{agui_post, http_json, http_sse, AgentSpec, Harness, HarnessOptions, HttpOptions};
use serde_json::{json, Value};

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

async fn harness(bind: &str) -> Harness {
    Harness::start(HarnessOptions {
        agents: vec![AgentSpec::worker("worker")],
        delegators: vec![],
        http: Some(HttpOptions {
            bind: bind.to_string(),
            token: "s3cret-token".to_string(),
            cors_origins: vec!["https://ui.example".to_string()],
        }),
        codex_command: Some(env!("CARGO_BIN_EXE_fake_codex").to_string()),
        claude_command: None,
        hermes_command: None,
    })
    .await
    .unwrap()
}

fn run_input(text: &str, task_ref: Option<&str>) -> Value {
    let mut input = json!({
        "threadId": "t-1",
        "runId": "r-1",
        "messages": [{ "role": "user", "content": text }],
    });
    if let Some(task_ref) = task_ref {
        input["forwardedProps"] = json!({ "task_ref": task_ref });
    }
    input
}

fn event_types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| {
            e.get("type")
                .or_else(|| e.get("event").and_then(|ev| ev.get("type")))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn task_ref_from(events: &[Value]) -> Option<String> {
    events.iter().find_map(|e| {
        if e.get("type").and_then(Value::as_str) == Some("CUSTOM")
            && e.get("name").and_then(Value::as_str) == Some("hearth.task_ref")
        {
            e["value"]["task_ref"].as_str().map(str::to_string)
        } else {
            None
        }
    })
}

#[tokio::test]
async fn auth_is_required_end_to_end() {
    let bind = format!("127.0.0.1:{}", free_port());
    let h = harness(&bind).await;
    // No token → 401 on both a plain GET and the AG-UI POST.
    let (code, _, _) = http_json(&bind, "GET", "/v1/agents", None, None, None)
        .await
        .unwrap();
    assert_eq!(code, 401);
    let (code, _, _) = http_json(
        &bind,
        "POST",
        "/v1/agents/worker/agui",
        None,
        Some(&run_input("hi", None)),
        None,
    )
    .await
    .unwrap();
    assert_eq!(code, 401);
    // With the token, discovery works and CORS echoes an allowlisted origin.
    let (code, agents, headers) = http_json(
        &bind,
        "GET",
        "/v1/agents",
        Some("s3cret-token"),
        None,
        Some("https://ui.example"),
    )
    .await
    .unwrap();
    assert_eq!(code, 200);
    assert!(agents["agents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["name"] == json!("worker")));
    assert!(headers.iter().any(
        |(k, v)| k.eq_ignore_ascii_case("access-control-allow-origin") && v == "https://ui.example"
    ));
    let _ = h;
}

#[tokio::test]
async fn a_fresh_thread_creates_a_task_and_streams_a_run() {
    let bind = format!("127.0.0.1:{}", free_port());
    let _h = harness(&bind).await;
    let events = agui_post(
        &bind,
        "s3cret-token",
        "worker",
        &run_input("hello agent", None),
    )
    .await
    .unwrap();
    let types = event_types(&events);
    assert!(
        types.contains(&"RUN_STARTED".to_string()),
        "types: {types:?}"
    );
    assert!(types.contains(&"TEXT_MESSAGE_CONTENT".to_string()));
    assert!(types.contains(&"RUN_FINISHED".to_string()));
    assert!(
        task_ref_from(&events).is_some(),
        "a resumable task_ref is emitted"
    );
}

#[tokio::test]
async fn interrupt_then_resume_on_the_same_thread() {
    let bind = format!("127.0.0.1:{}", free_port());
    let _h = harness(&bind).await;
    // First run raises an approval → the SSE run ends after the permission
    // request (AG-UI interrupt lifecycle).
    let first = agui_post(
        &bind,
        "s3cret-token",
        "worker",
        &run_input("NEEDS_APPROVAL do the thing", None),
    )
    .await
    .unwrap();
    let names: Vec<String> = first
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("CUSTOM"))
        .filter_map(|e| e.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "hearth.permission_request"),
        "the interrupt surfaces a permission request: {names:?}"
    );
    let task_ref = task_ref_from(&first).expect("task_ref for resume");

    // The answer is a NEW RunAgentInput on the same thread carrying the ref.
    let second = agui_post(
        &bind,
        "s3cret-token",
        "worker",
        &run_input("allow", Some(&task_ref)),
    )
    .await
    .unwrap();
    let types = event_types(&second);
    assert!(
        types.contains(&"RUN_STARTED".to_string()) && types.contains(&"RUN_FINISHED".to_string()),
        "the resume is a fresh run that finishes: {types:?}"
    );
}

#[tokio::test]
async fn detach_reattach_replays_losslessly_for_two_uis() {
    let bind = format!("127.0.0.1:{}", free_port());
    let _h = harness(&bind).await;
    // Run a task to completion.
    let first = agui_post(&bind, "s3cret-token", "worker", &run_input("hello", None))
        .await
        .unwrap();
    let task_ref = task_ref_from(&first).unwrap();

    // Two independent UIs replay the same task's events from the start
    // (disconnect/reattach loses nothing — the guest log is the truth).
    let path = format!("/v1/tasks/{task_ref}/events?cursor=");
    let ui_a = http_sse(&bind, "GET", &path, Some("s3cret-token"), None)
        .await
        .unwrap();
    let ui_b = http_sse(&bind, "GET", &path, Some("s3cret-token"), None)
        .await
        .unwrap();
    let types_a = event_types(&ui_a);
    let types_b = event_types(&ui_b);
    assert!(types_a.contains(&"RUN_STARTED".to_string()));
    assert!(types_a.contains(&"RUN_FINISHED".to_string()));
    assert_eq!(types_a, types_b, "both UIs see the identical replay");
}
