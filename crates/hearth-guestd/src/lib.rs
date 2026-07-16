//! hearth-guestd — one daemon inside every agent VM (docs/agent-plane.md §2).
//! Machine-plane duties (boot report, readiness, heartbeat) plus agent-plane
//! duties (drive the agent CLIs, own the durable task registry, deliver
//! wake-ups). A musl-linked static binary so it drops into any image.

pub mod adapter;
pub mod engine;
pub mod report;
pub mod server;
pub mod shim;
pub mod store;
pub mod transport;
pub mod vsock_io;

use crate::adapter::claude::ClaudeAdapter;
use crate::adapter::codex::CodexAdapter;
use crate::adapter::Adapter;
use crate::engine::Engine;
use crate::store::Store;
use crate::transport::Transport;
use anyhow::Result;
use camino::Utf8PathBuf;
use std::collections::HashMap;
use std::sync::Arc;

/// Which CLIs to wire, with their binary names. Codex is the day-1 vertical
/// (§2.2); claude follows in Phase 5. Commands are overridable so tests point
/// at fakes.
pub struct AdapterConfig {
    pub codex_command: String,
    pub claude_command: Option<String>,
}

impl AdapterConfig {
    pub fn codex_only(codex_command: &str) -> Self {
        Self {
            codex_command: codex_command.to_string(),
            claude_command: None,
        }
    }
}

/// Assemble the engine with codex (and, if configured, claude).
pub fn build_engine(state_dir: &Utf8PathBuf, codex_command: &str) -> Result<Arc<Engine>> {
    build_engine_with(state_dir, AdapterConfig::codex_only(codex_command))
}

pub fn build_engine_with(state_dir: &Utf8PathBuf, cfg: AdapterConfig) -> Result<Arc<Engine>> {
    let store = Arc::new(Store::new(state_dir, 256 * 1024, 64)?);
    let mut adapters: HashMap<String, Arc<dyn Adapter>> = HashMap::new();
    let codex = Arc::new(CodexAdapter::new(&cfg.codex_command));
    adapters.insert(codex.name().to_string(), codex);
    if let Some(claude_command) = &cfg.claude_command {
        let claude = Arc::new(ClaudeAdapter::new(claude_command));
        adapters.insert(claude.name().to_string(), claude);
    }
    Ok(Engine::new(store, adapters))
}

/// Wire the transport listeners and guest→host loops onto an engine. Returns
/// once the guestd port server is bound (it then runs until the process ends).
pub async fn serve(
    transport: Transport,
    engine: Arc<Engine>,
    boot_id: String,
    hostname: String,
    addrs: Vec<String>,
) -> Result<()> {
    engine.recover().await?;
    let agents = report::probe_agents(&engine).await;

    // Boot report + heartbeat (port 1025).
    tokio::spawn(report::report_loop(
        transport.clone(),
        Arc::clone(&engine),
        boot_id,
        agents,
        hostname,
        addrs,
    ));
    // Upcall / outbox delivery (port 1026).
    tokio::spawn(report::upcall_loop(transport.clone(), Arc::clone(&engine)));

    // Task-verb server (port 1027, host→guest).
    let engine_for_server = Arc::clone(&engine);
    transport
        .serve_guest_port(move |stream| {
            let engine = Arc::clone(&engine_for_server);
            tokio::spawn(async move {
                if let Err(err) = server::serve_connection(engine, stream).await {
                    tracing::warn!(error = %err, "task-verb connection failed");
                }
            });
        })
        .await
}
