//! The machine-plane boot-report loop (docs/agent-plane.md §2.1) and the
//! agent-plane upcall/outbox loop (§7.2), both guest→host.
//!
//! Boot report (port 1025): on connect, send hello + report; then heartbeat
//! periodically and re-report on change. hearthd acks each report; an ack with
//! `restored: true` means this boot follows a snapshot restore, so guestd
//! rotates task incarnations (§3.4). Both loops reconnect with backoff, so a
//! hearthd restart or transient drop self-heals.

use crate::engine::Engine;
use crate::transport::Transport;
use anyhow::{Context, Result};
use hearth_agent_proto::task::Delivery;
use hearth_agent_proto::{
    read_line_capped, AgentDecl, BootReport, GuestFrame, Heartbeat, Hello, HelloChannel, HostFrame,
    PORT_AGENT, PORT_REPORT,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{info, warn};

const HEARTBEAT: Duration = Duration::from_secs(15);
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
const LINE_CAP: usize = 64 * 1024;

/// Run the boot-report loop forever (reconnecting on drop).
pub async fn report_loop(
    transport: Transport,
    engine: Arc<Engine>,
    boot_id: String,
    agents: Vec<AgentDecl>,
    hostname: String,
    addrs: Vec<String>,
) {
    loop {
        if let Err(err) = report_once(
            &transport,
            &engine,
            &boot_id,
            &agents,
            &hostname,
            &addrs,
        )
        .await
        {
            warn!(error = %err, "boot-report connection dropped; reconnecting");
        }
        tokio::time::sleep(RECONNECT_BACKOFF).await;
    }
}

async fn report_once(
    transport: &Transport,
    engine: &Arc<Engine>,
    boot_id: &str,
    agents: &[AgentDecl],
    hostname: &str,
    addrs: &[String],
) -> Result<()> {
    let stream = transport
        .dial_host(PORT_REPORT)
        .await
        .context("dial boot-report port")?;
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);

    let frame = GuestFrame {
        hello: Some(Hello::new("guestd", env!("CARGO_PKG_VERSION"))),
        report: Some(BootReport {
            ready: true,
            addrs: addrs.to_vec(),
            hostname: hostname.to_string(),
            agents: agents.to_vec(),
            boot_id: boot_id.to_string(),
        }),
        heartbeat: None,
    };
    write
        .write_all((serde_json::to_string(&frame)? + "\n").as_bytes())
        .await?;

    // First ack: may carry restored=true (rotate incarnations, §3.4).
    let mut line = String::new();
    if reader.read_line(&mut line).await? != 0 {
        if let Ok(host) = serde_json::from_str::<HostFrame>(&line) {
            if host.ack.map(|a| a.restored).unwrap_or(false) {
                info!("boot follows a restore; rotating task incarnations");
                if let Err(err) = engine.rotate_incarnation() {
                    warn!(error = %err, "failed to rotate incarnations after restore");
                }
            }
        }
    }
    info!(boot_id, "boot report delivered");

    // Heartbeat until the connection drops.
    loop {
        tokio::time::sleep(HEARTBEAT).await;
        let frame = GuestFrame {
            hello: None,
            report: None,
            heartbeat: Some(Heartbeat {
                ts: crate::store::now(),
            }),
        };
        write
            .write_all((serde_json::to_string(&frame)? + "\n").as_bytes())
            .await?;
    }
}

/// Run the upcall/outbox loop forever: deliver every pending outbox entry to
/// agentd (port 1026, `channel: "upcall"`), retrying with backoff until acked;
/// replay on every reconnect (§7.2). agentd resolves wake-up authority from
/// its ledger — this side only reports facts.
pub async fn upcall_loop(transport: Transport, engine: Arc<Engine>) {
    loop {
        match deliver_outbox(&transport, &engine).await {
            Ok(delivered) if delivered => {}
            Ok(_) => {
                // Nothing pending: wait for the next enqueue or a periodic poke.
                tokio::select! {
                    _ = engine.outbox_notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
            Err(err) => {
                warn!(error = %err, "upcall delivery failed; retrying");
                tokio::time::sleep(RECONNECT_BACKOFF).await;
            }
        }
    }
}

/// Attempt to flush the whole outbox in one connection. Returns whether any
/// entry was delivered (so the caller loops promptly to drain the rest).
async fn deliver_outbox(transport: &Transport, engine: &Arc<Engine>) -> Result<bool> {
    let pending = engine.store.outbox_pending()?;
    if pending.is_empty() {
        return Ok(false);
    }
    let stream = transport.dial_host(PORT_AGENT).await.context("dial agentd upcall port")?;
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);

    // Hello selecting the upcall channel (§2.4 / §7.2).
    let mut hello = Hello::new("guestd", env!("CARGO_PKG_VERSION"));
    hello.channel = Some(HelloChannel::Upcall);
    write
        .write_all((serde_json::to_string(&hello)? + "\n").as_bytes())
        .await?;

    let mut delivered = false;
    for delivery in pending {
        let framed = frame_delivery(engine, &delivery);
        write
            .write_all((serde_json::to_string(&framed)? + "\n").as_bytes())
            .await?;
        // Wait for agentd's ack of this delivery before deleting it.
        let line = read_line_capped(&mut reader, LINE_CAP)
            .await?
            .context("agentd closed the upcall stream before acking")?;
        let ack: UpcallAck = serde_json::from_str(&line).context("parse upcall ack")?;
        if ack.acked && ack.delivery_id == delivery.delivery_id {
            engine.store.outbox_ack(&delivery.delivery_id)?;
            delivered = true;
        } else {
            // This one is not (yet) deliverable — e.g. its initiator VM is down,
            // or a target mismatch. Skip it and keep flushing the rest, so one
            // permanently-stuck entry never starves newer wake-ups (it retries
            // on the next connection).
            warn!(delivery = %delivery.delivery_id, "agentd did not ack; will retry");
            continue;
        }
    }
    Ok(delivered)
}

/// The upcall payload: the delivery plus the initiator thread agentd needs to
/// route the wake-up (from the task's advisory initiator copy; agentd still
/// authorizes from its ledger, never from this).
fn frame_delivery(engine: &Arc<Engine>, delivery: &Delivery) -> serde_json::Value {
    let initiator_thread = engine
        .status(&delivery.task_id)
        .ok()
        .and_then(|s| s.initiator)
        .and_then(|i| i.thread_id);
    json!({
        "upcall": {
            "delivery_id": delivery.delivery_id,
            "task_id": delivery.task_id,
            "transition": delivery.transition,
            "detail": delivery.detail,
            "initiator_thread": initiator_thread,
            "created": delivery.created,
        }
    })
}

#[derive(serde::Deserialize)]
struct UpcallAck {
    acked: bool,
    #[serde(default)]
    delivery_id: String,
}

/// Enumerate this guest's declared agents by probing each adapter (§2.1/§2.2).
pub async fn probe_agents(engine: &Arc<Engine>) -> Vec<AgentDecl> {
    let mut decls = Vec::new();
    for name in engine.adapters() {
        // The engine holds the adapters; re-probe by name via a task-less call.
        match engine.probe_agent(&name).await {
            Ok(version) => decls.push(AgentDecl {
                name,
                cli_version: Some(version),
                ok: true,
                error: None,
            }),
            Err(err) => decls.push(AgentDecl {
                name,
                cli_version: None,
                ok: false,
                error: Some(err.to_string()),
            }),
        }
    }
    decls
}
