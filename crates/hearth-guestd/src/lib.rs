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
use crate::adapter::hermes::{HermesAdapter, ProcessIdentity};
use crate::adapter::Adapter;
use crate::engine::Engine;
use crate::store::Store;
use crate::transport::Transport;
use anyhow::Result;
use camino::Utf8PathBuf;
use std::collections::HashMap;
use std::sync::Arc;

/// The same build version used by every shipped Hearth binary.
pub const VERSION: &str = hearth_proto::VERSION;

/// Which CLIs to wire, with their binary names. Commands are overridable so
/// tests point at fakes; absent commands do not register an adapter.
pub struct AdapterConfig {
    pub codex_command: Option<String>,
    pub claude_command: Option<String>,
    pub hermes: Option<HermesConfig>,
}

pub struct HermesConfig {
    pub command: String,
    pub identity: Option<ProcessIdentity>,
}

impl HermesConfig {
    pub fn current_user(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            identity: None,
        }
    }

    /// The fixed workload identity declared by vm-base.
    pub fn agent_user(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            identity: Some(ProcessIdentity {
                uid: 1000,
                gid: 1000,
                home: "/home/agent".into(),
            }),
        }
    }
}

impl AdapterConfig {
    pub fn codex_only(codex_command: &str) -> Self {
        Self {
            codex_command: Some(codex_command.to_string()),
            claude_command: None,
            hermes: None,
        }
    }
}

/// Assemble the engine with codex (and any other configured adapters).
pub fn build_engine(state_dir: &Utf8PathBuf, codex_command: &str) -> Result<Arc<Engine>> {
    build_engine_with(state_dir, AdapterConfig::codex_only(codex_command))
}

pub fn build_engine_with(state_dir: &Utf8PathBuf, cfg: AdapterConfig) -> Result<Arc<Engine>> {
    let store = Arc::new(Store::new(state_dir, 256 * 1024, 64)?);
    let mut adapters: HashMap<String, Arc<dyn Adapter>> = HashMap::new();
    if let Some(codex_command) = &cfg.codex_command {
        let codex = Arc::new(CodexAdapter::new(codex_command));
        adapters.insert(codex.name().to_string(), codex);
    }
    if let Some(claude_command) = &cfg.claude_command {
        let claude = Arc::new(ClaudeAdapter::new(claude_command));
        adapters.insert(claude.name().to_string(), claude);
    }
    if let Some(hermes_cfg) = cfg.hermes {
        let mut hermes = HermesAdapter::new(hermes_cfg.command);
        if let Some(identity) = hermes_cfg.identity {
            hermes = hermes.run_as(identity);
        }
        let hermes = Arc::new(hermes);
        adapters.insert(hermes.name().to_string(), hermes);
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
