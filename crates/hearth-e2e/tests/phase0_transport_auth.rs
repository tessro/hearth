//! Phase 0 acceptance (docs/agent-plane.md §11): transport and authorization
//! truth. The agent-in-charge verb channel works over the CHV-hybrid socket; a
//! restricted-uid client runs exactly the allowlisted verbs and nothing else;
//! the broker never crosses the guest channel.

use hearth_agent_proto::{
    read_line_capped, AgentRequest, AgentVerb, Hello, AGENT_PROTOCOL_VERSION, MAX_LINE_BYTES,
};
use hearth_e2e::{machine_verb, AgentSpec, Harness, HarnessOptions};
use hearth_proto::{Response, Verb};
use serde_json::{json, Map, Value};
use tokio::io::AsyncWriteExt;
use ulid::Ulid;

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

#[tokio::test]
async fn agent_in_charge_verb_channel_works_over_hybrid_socket() {
    let h = Harness::start(opts()).await.unwrap();
    // The agent-in-charge's `<vm>.sock_1024` accepts a machine-plane `ls`.
    let mut stream = h.guest_verb_channel("boss").await.unwrap();
    let resp = machine_verb(&mut stream, Verb::Ls, Map::new())
        .await
        .unwrap();
    assert!(resp.ok, "ls over the guest verb channel should succeed");
    let services = resp.result.unwrap();
    let names: Vec<&str> = services["services"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["hostname"].as_str())
        .collect();
    assert!(names.contains(&"boss") && names.contains(&"worker"));
}

#[tokio::test]
async fn broker_verbs_are_denied_on_the_guest_channel() {
    let h = Harness::start(opts()).await.unwrap();
    // A guest must never obtain fds to other VMs' sockets: the broker verbs are
    // refused on the guest verb channel even for the agent-in-charge.
    let mut stream = h.guest_verb_channel("boss").await.unwrap();
    let mut args = Map::new();
    args.insert("name".to_string(), Value::String("worker".to_string()));
    let resp = machine_verb(&mut stream, Verb::GuestConnect, args)
        .await
        .unwrap();
    assert!(!resp.ok);
    assert_eq!(resp.error.unwrap().code, "verb.denied");
}

#[tokio::test]
async fn non_agent_in_charge_verb_channel_is_dropped() {
    let h = Harness::start(opts()).await.unwrap();
    // A non-agent-in-charge VM's verb channel accepts the connection but
    // dispatches nothing — hearthd closes it without a response.
    let mut stream = h.guest_verb_channel("worker").await.unwrap();
    let result = machine_verb(&mut stream, Verb::Ls, Map::new()).await;
    assert!(
        result.is_err(),
        "worker is not the agent-in-charge; its verb channel must not dispatch"
    );
}

#[tokio::test]
async fn per_uid_policy_allows_the_allowlist_and_denies_the_rest() {
    let h = Harness::start(opts()).await.unwrap();
    // The test uid is granted the agentd allowlist. `agent-endpoints` is on it.
    let ok = h.hearthd(Verb::AgentEndpoints, Map::new()).await.unwrap();
    assert!(ok.ok, "agent-endpoints is allowlisted");
    // `destroy` is not on the allowlist → verb.denied, before any work.
    let mut args = Map::new();
    args.insert("name".to_string(), Value::String("worker".to_string()));
    let denied = h.hearthd(Verb::Destroy, args).await.unwrap();
    assert!(!denied.ok);
    assert_eq!(denied.error.unwrap().code, "verb.denied");
}

#[tokio::test]
async fn agent_endpoints_lists_only_agent_enabled_vms() {
    let h = Harness::start(opts()).await.unwrap();
    let resp = h.hearthd(Verb::AgentEndpoints, Map::new()).await.unwrap();
    let agents = resp.result.unwrap();
    let names: Vec<&str> = agents["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["hostname"].as_str())
        .collect();
    assert!(names.contains(&"boss") && names.contains(&"worker"));
}

#[tokio::test]
async fn guestd_requires_the_agent_plane_hello_before_task_verbs() {
    let h = Harness::start(opts()).await.unwrap();
    let mut stream = h.guest_connect_transport_only("worker").await.unwrap();
    let request = AgentRequest::new(Ulid::new().to_string(), AgentVerb::Ping, Map::new());
    stream
        .write_all((serde_json::to_string(&request).unwrap() + "\n").as_bytes())
        .await
        .unwrap();
    let line = read_line_capped(&mut stream, MAX_LINE_BYTES)
        .await
        .unwrap()
        .unwrap();
    let response: Response = serde_json::from_str(&line).unwrap();
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "protocol.hello_required");
}

#[tokio::test]
async fn guestd_rejects_agent_plane_protocol_skew() {
    let h = Harness::start(opts()).await.unwrap();
    let mut stream = h.guest_connect_transport_only("worker").await.unwrap();
    let mut hello = Hello::new("hearthctl-agent", "test");
    hello.proto = AGENT_PROTOCOL_VERSION + 1;
    stream
        .write_all((serde_json::to_string(&hello).unwrap() + "\n").as_bytes())
        .await
        .unwrap();
    let line = read_line_capped(&mut stream, MAX_LINE_BYTES)
        .await
        .unwrap()
        .unwrap();
    let response: Response = serde_json::from_str(&line).unwrap();
    assert!(!response.ok, "{}", json!(response.result));
    assert_eq!(response.error.unwrap().code, "protocol.version_mismatch");
}
