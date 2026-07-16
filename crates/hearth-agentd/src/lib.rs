//! hearth-agentd — the unprivileged host daemon of the agent plane
//! (docs/agent-plane.md §4). Terminates AG-UI over HTTP for UIs, serves one
//! MCP server for agent-to-agent delegation, relays everything to guestds over
//! the hearthd broker, enforces delegation policy, and audits. Content-stateless
//! (§4.4): it persists only the delegation ledger, never task content.

pub mod config;
pub mod control;
pub mod core;
pub mod hearthd;
pub mod http;
pub mod ledger;
pub mod mcp;
pub mod refs;
pub mod relay;
pub mod wakeups;

use crate::config::Config;
use crate::core::Agentd;
use crate::ledger::Ledger;
use crate::refs::RefKeys;
use anyhow::{bail, Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Assemble the agentd core from config + loaded secrets.
pub async fn build(cfg: Config) -> Result<Arc<Agentd>> {
    let ledger = Ledger::open(&cfg.ledger_dir).context("open delegation ledger")?;
    let current = config::read_secret(&cfg.ref_key_file)
        .await?
        .unwrap_or_else(|| {
            warn!("no task-ref key configured; using an ephemeral key (refs won't survive restart)");
            ephemeral_key()
        });
    let previous = config::read_secret(&cfg.ref_key_prev_file).await?;
    let keys = RefKeys {
        current,
        previous,
        ttl_secs: cfg.ref_ttl_secs,
    };
    Ok(Agentd::new(cfg, keys, ledger))
}

/// Run agentd: control socket + MCP/upcall listeners + optional HTTP leg.
pub async fn run(agentd: Arc<Agentd>) -> Result<()> {
    let mcp = mcp::McpServer::new(Arc::clone(&agentd));
    let listeners = wakeups::Listeners::new(Arc::clone(&agentd), Arc::clone(&mcp));

    // Poll hearthd for agent VMs and (re)bind their brokered listeners.
    {
        let listeners = Arc::clone(&listeners);
        tokio::spawn(async move {
            loop {
                if let Err(err) = listeners.refresh().await {
                    warn!(error = %err, "listener refresh failed");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    // Control socket (§4.1).
    let control_listener = bind_control(&agentd.cfg.control_socket).await?;
    {
        let agentd = Arc::clone(&agentd);
        tokio::spawn(async move {
            if let Err(err) = control::serve(agentd, control_listener).await {
                warn!(error = %err, "control socket stopped");
            }
        });
    }

    // HTTP leg (§4.2), unless disabled.
    if !agentd.cfg.no_http && !agentd.cfg.http_bind.is_empty() {
        let http = load_http_config(&agentd.cfg).await?;
        let bind = &agentd.cfg.http_bind;
        if bind.starts_with("0.0.0.0") {
            bail!(
                "refusing to bind AG-UI HTTP on {bind}: this surface is RCE-by-design; \
                 bind loopback or a tailnet address explicitly"
            );
        }
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("bind AG-UI HTTP {bind}"))?;
        info!(bind = %bind, "AG-UI HTTP endpoint listening");
        let agentd = Arc::clone(&agentd);
        tokio::spawn(async move {
            if let Err(err) = http::serve(agentd, listener, http).await {
                warn!(error = %err, "http leg stopped");
            }
        });
    }

    info!("hearth-agentd ready");
    // Park forever; the tasks above own the work.
    std::future::pending::<()>().await;
    Ok(())
}

async fn load_http_config(cfg: &Config) -> Result<Arc<http::HttpConfig>> {
    let token = config::read_secret(&cfg.token_file)
        .await?
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "AG-UI HTTP is enabled but no bearer token is configured; set \
                 --token-file (LoadCredential) or pass --no-http"
            )
        })?;
    Ok(Arc::new(http::HttpConfig {
        token,
        cors_origins: cfg.cors_list(),
    }))
}

async fn bind_control(path: &camino::Utf8Path) -> Result<tokio::net::UnixListener> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    match tokio::fs::remove_file(path.as_std_path()).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("remove stale control socket {path}")),
    }
    let listener = tokio::net::UnixListener::bind(path.as_str())
        .with_context(|| format!("bind control socket {path}"))?;
    set_control_permissions(path)?;
    Ok(listener)
}

/// `0660 root:hearth` per §4.1, matching hearthd's socket.
fn set_control_permissions(path: &camino::Utf8Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path.as_str(), std::fs::Permissions::from_mode(0o660))?;
    Ok(())
}

/// A process-lifetime random key when none is configured. Deterministic
/// randomness is unavailable without a dep; use pid+addr entropy (fine for a
/// dev fallback that is explicitly warned about).
fn ephemeral_key() -> Vec<u8> {
    let mut key = Vec::new();
    key.extend_from_slice(&std::process::id().to_le_bytes());
    let stack = 0u8;
    key.extend_from_slice(&(&stack as *const u8 as usize).to_le_bytes());
    key.extend_from_slice(ulid::Ulid::new().to_string().as_bytes());
    key
}
