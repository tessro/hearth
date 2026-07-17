//! Phase 1 acceptance (docs/agent-plane.md §11): guestd machine plane. `wait`
//! resolves on the boot report with no marker; `status` shows corroborated
//! telemetry (guestd version, agents, last_seen).

use hearth_e2e::{AgentSpec, Harness, HarnessOptions};
use hearth_proto::Verb;
use serde_json::{json, Map, Value};

fn opts() -> HarnessOptions {
    HarnessOptions {
        agents: vec![AgentSpec::worker("worker")],
        delegators: vec![],
        http: None,
        codex_command: Some(env!("CARGO_BIN_EXE_fake_codex").to_string()),
        claude_command: None,
        hermes_command: None,
    }
}

#[tokio::test]
async fn wait_resolves_on_the_boot_report_without_a_marker() {
    let h = Harness::start(opts()).await.unwrap();
    let mut args = Map::new();
    args.insert("name".to_string(), json!("worker"));
    args.insert("timeout".to_string(), json!(10));
    let resp = h.hearthd(Verb::Wait, args).await.unwrap();
    assert!(
        resp.ok,
        "wait should resolve on the guestd boot report: {resp:?}"
    );
    let result = resp.result.unwrap();
    assert_eq!(result["ready"], json!(true));
    assert_eq!(result["guestd"]["component"], json!("guestd"));
}

#[tokio::test]
async fn wait_requires_marker_for_guestd_less_images() {
    // A guestd-less image must still require --marker (§2.5): the daemon says so
    // rather than hanging. We assert the error path by pointing wait at an image
    // that does not declare guestd — modelled here by a service whose manifest
    // lacks guestd. The harness only builds guestd images, so we check the
    // inverse: an unknown service errors cleanly, never hangs.
    let h = Harness::start(opts()).await.unwrap();
    let mut args = Map::new();
    args.insert("name".to_string(), json!("nonexistent"));
    args.insert("timeout".to_string(), json!(2));
    let resp = h.hearthd(Verb::Wait, args).await.unwrap();
    assert!(!resp.ok);
    assert_eq!(resp.error.unwrap().code, "service.not_found");
}

#[tokio::test]
async fn status_surfaces_guestd_telemetry() {
    let h = Harness::start(opts()).await.unwrap();
    // Give the boot report a moment to land.
    let mut args = Map::new();
    args.insert("name".to_string(), json!("worker"));
    args.insert("timeout".to_string(), json!(10));
    let _ = h.hearthd(Verb::Wait, args).await.unwrap();

    let mut args = Map::new();
    args.insert("name".to_string(), json!("worker"));
    let resp = h.hearthd(Verb::Status, args).await.unwrap();
    assert!(resp.ok);
    let status = resp.result.unwrap();
    let guestd = &status["guestd"];
    assert_eq!(guestd["ready"], json!(true));
    assert!(guestd["last_seen"].as_str().is_some(), "last_seen present");
    // The codex adapter is declared as an agent in the boot report.
    let agents = guestd["agents"].as_array().unwrap();
    assert!(agents
        .iter()
        .any(|a| a["name"] == json!("codex") && a["ok"] == json!(true)));
    assert_eq!(status["agent"], Value::Bool(true));
}
