//! Per-VM hybrid vsock listeners (docs/agent-plane.md §6).
//!
//! CHV's vsock device is the Firecracker-style *hybrid* model: there is no
//! host-side `AF_VSOCK` at all. A guest connecting to CID 2 port P lands on
//! whoever listens on the host unix socket `/run/hearth/vsock/<vm>.sock_P`.
//! hearthd binds `_1025` for boot reports, readiness, heartbeats, and the
//! restore signal for each running VM.
//!
//! Port 1026 (`_1026`, MCP + upcalls) is bound on demand through the broker
//! verb `guest-listener` and fd-passed to agentd; it is not served here.
//!
//! This replaces the previous host-side `AF_VSOCK` listener, which was the
//! vhost-vsock model and never saw a hybrid guest's connections (the §6
//! migration note's bug).

use crate::Daemon;
use anyhow::{Context, Result};
use hearth_agent_proto::{
    read_line_capped, GuestFrame, HostFrame, ReportAck, MAX_LINE_BYTES, PORT_AGENT, PORT_REPORT,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tracing::{info, warn};

impl<H: crate::host::Host + 'static> Daemon<H> {
    /// Bind the per-VM hybrid listeners for a (about-to-be-)running service.
    /// Idempotent: listeners persist across guest reconnects and VM restarts;
    /// the machine-owned listener is torn down by
    /// [`Daemon::drop_guest_channels`].
    pub(crate) async fn ensure_guest_channels(&self, name: &str) -> Result<()> {
        if self.cfg.disable_vsock {
            return Ok(());
        }
        let mut channels = self.channels.lock().await;
        if channels.contains_key(name) {
            return Ok(());
        }
        let path = self.cfg.vm_vsock_port_socket(name, PORT_REPORT);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).context("remove stale guest listener socket"),
        }
        let listener = UnixListener::bind(path.as_str())
            .with_context(|| format!("bind guest listener {path}"))?;
        let daemon = self.clone();
        let service = name.to_string();
        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let daemon = daemon.clone();
                        let service = service.clone();
                        tokio::spawn(async move {
                            if let Err(err) = daemon.handle_guest_reports(&service, stream).await {
                                warn!(port = PORT_REPORT, error = %err, "guest channel failed");
                            }
                        });
                    }
                    Err(err) => {
                        warn!(service = %service, port = PORT_REPORT, error = %err, "guest listener accept failed");
                        break;
                    }
                }
            }
        });
        channels.insert(name.to_string(), vec![handle]);
        info!(service = %name, "guest vsock channels bound");
        Ok(())
    }

    /// Tear down hearthd's machine-plane listener on stop/destroy.
    ///
    /// The agentd-brokered `_1026` listener must survive a stop/start. Agentd
    /// owns its open fd and tracks it by stable VM id; unlinking the path here
    /// leaves agentd accepting on an unreachable inode and makes every MCP
    /// shim connection reset until agentd restarts.
    pub(crate) async fn drop_guest_channels(&self, name: &str) {
        let mut channels = self.channels.lock().await;
        if let Some(handles) = channels.remove(name) {
            for handle in handles {
                handle.abort();
            }
        }
        drop(channels);
        let _ = tokio::fs::remove_file(self.cfg.vm_vsock_port_socket(name, PORT_REPORT)).await;
    }

    /// Remove agentd's brokered listener path when a stable VM id is destroyed.
    /// A later VM gets a new id, so stop/start must retain this path while
    /// destroy must not leave it behind.
    pub(crate) async fn drop_agent_channel(&self, id: &str) {
        let _ = tokio::fs::remove_file(self.cfg.vm_vsock_port_socket(id, PORT_AGENT)).await;
    }

    /// Bind channels for every already-running service (daemon startup).
    pub(crate) async fn bind_running_guest_channels(&self) {
        let Ok(reg) = self.registry().await else {
            return;
        };
        for svc in reg.services.values() {
            if self.is_running(&svc.id).await {
                if let Err(err) = self.ensure_guest_channels(&svc.id).await {
                    warn!(hostname = %svc.hostname, id = %svc.id, error = %err, "failed to bind guest channels");
                }
            }
        }
    }

    /// The boot-report channel on `_1025` (§2.1). Every report is acked; the
    /// ack carries `restored: true` exactly once after a `restore`, telling
    /// guestd to rotate task incarnations (§3.4).
    async fn handle_guest_reports(&self, name: &str, mut stream: UnixStream) -> Result<()> {
        let result = self.report_loop(name, &mut stream).await;
        self.guests.disconnected(name);
        result
    }

    async fn report_loop(&self, name: &str, stream: &mut UnixStream) -> Result<()> {
        loop {
            let Some(line) = read_line_capped(stream, MAX_LINE_BYTES).await? else {
                return Ok(());
            };
            if line.trim().is_empty() {
                continue;
            }
            let frame: GuestFrame = match serde_json::from_str(&line) {
                Ok(frame) => frame,
                Err(err) => {
                    warn!(service = %name, error = %err, "unparseable guest report frame");
                    continue;
                }
            };
            if let Some(report) = frame.report {
                let restored = self.guests.restore_pending(name);
                let ready = report.ready;
                self.guests
                    .update_report(name, frame.hello.as_ref(), report);
                let ack = HostFrame {
                    ack: Some(ReportAck { restored }),
                };
                stream
                    .write_all((serde_json::to_string(&ack)? + "\n").as_bytes())
                    .await?;
                if restored {
                    // Only consumed once the ack actually went out; a reconnect
                    // before that re-delivers the signal.
                    self.guests.take_pending_restore(name);
                }
                info!(
                    caller_transport = "vsock",
                    caller_service = %name,
                    ready,
                    restored,
                    "guest boot report"
                );
            } else if frame.heartbeat.is_some() {
                self.guests.heartbeat(name);
            }
        }
    }
}
