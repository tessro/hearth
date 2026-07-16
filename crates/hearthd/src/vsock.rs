//! Per-VM hybrid vsock listeners (docs/agent-plane.md §6).
//!
//! CHV's vsock device is the Firecracker-style *hybrid* model: there is no
//! host-side `AF_VSOCK` at all. A guest connecting to CID 2 port P lands on
//! whoever listens on the host unix socket `/run/hearth/vsock/<vm>.sock_P`.
//! hearthd binds two of those per running VM:
//!
//! - `_1024` — the machine-plane verb channel (agent-in-charge only, the
//!   existing contract; identity is the socket path, not a token).
//! - `_1025` — boot report / readiness / heartbeat / restore signal.
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
    PORT_VERBS,
};
use hearth_proto::{Request, Response, Verb};
use serde_json::Value;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tracing::{info, warn};

impl<H: crate::host::Host + 'static> Daemon<H> {
    /// Bind the per-VM hybrid listeners for a (about-to-be-)running service.
    /// Idempotent: listeners persist across guest reconnects and VM restarts;
    /// they are only torn down by [`Daemon::drop_guest_channels`].
    pub(crate) async fn ensure_guest_channels(&self, name: &str) -> Result<()> {
        if self.cfg.disable_vsock {
            return Ok(());
        }
        let mut channels = self.channels.lock().await;
        if channels.contains_key(name) {
            return Ok(());
        }
        let mut handles = Vec::new();
        for port in [PORT_VERBS, PORT_REPORT] {
            let path = self.cfg.vm_vsock_port_socket(name, port);
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
            handles.push(tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            let conn = daemon.guest_conn(port, service.clone(), stream);
                            tokio::spawn(async move {
                                if let Err(err) = conn.await {
                                    warn!(port, error = %err, "guest channel failed");
                                }
                            });
                        }
                        Err(err) => {
                            warn!(service = %service, port, error = %err, "guest listener accept failed");
                            break;
                        }
                    }
                }
            }));
        }
        channels.insert(name.to_string(), handles);
        info!(service = %name, "guest vsock channels bound");
        Ok(())
    }

    /// Tear down a service's hybrid listeners (stop/destroy). Also unlinks the
    /// agentd-brokered `_1026` socket: it is bound by hearthd on agentd's
    /// `guest-listener` request and not tracked here, so if it survived a
    /// destroy a service recreated under the same name — possibly *without*
    /// `agent = true` — would silently inherit the dead VM's agent-plane
    /// identity (§8). agentd's stale listener simply stops receiving
    /// connections once the socket is gone.
    pub(crate) async fn drop_guest_channels(&self, name: &str) {
        let mut channels = self.channels.lock().await;
        if let Some(handles) = channels.remove(name) {
            for handle in handles {
                handle.abort();
            }
        }
        drop(channels);
        for port in [PORT_VERBS, PORT_REPORT, PORT_AGENT] {
            let _ = tokio::fs::remove_file(self.cfg.vm_vsock_port_socket(name, port)).await;
        }
    }

    /// Bind channels for every already-running service (daemon startup).
    pub(crate) async fn bind_running_guest_channels(&self) {
        let Ok(reg) = self.registry().await else {
            return;
        };
        for svc in reg.services.values() {
            if self.is_running(&svc.name).await {
                if let Err(err) = self.ensure_guest_channels(&svc.name).await {
                    warn!(service = %svc.name, error = %err, "failed to bind guest channels");
                }
            }
        }
    }

    /// One guest connection, type-erased. The verb handler can dispatch
    /// `start`, which re-enters `ensure_guest_channels`, which spawns this —
    /// boxing at this boundary breaks the otherwise-infinite future type.
    fn guest_conn(
        &self,
        port: u32,
        service: String,
        stream: UnixStream,
    ) -> futures_util::future::BoxFuture<'static, Result<()>> {
        let daemon = self.clone();
        Box::pin(async move {
            if port == PORT_VERBS {
                daemon.handle_guest_verbs(&service, stream).await
            } else {
                daemon.handle_guest_reports(&service, stream).await
            }
        })
    }

    /// The machine-plane verb channel on `_1024`. The connecting VM *is* the
    /// socket path; only the agent-in-charge gets dispatch (existing
    /// contract), and the broker verbs never cross this channel — a guest
    /// must not be able to obtain fds to other VMs' sockets.
    async fn handle_guest_verbs(&self, name: &str, mut stream: UnixStream) -> Result<()> {
        let reg = self.registry().await?;
        let Ok(svc) = reg.get(name) else {
            warn!(service = %name, "vsock verb connection for unknown service; dropping");
            return Ok(());
        };
        if !svc.is_agent_in_charge {
            warn!(service = %name, "dropping vsock verb connection: not the agent-in-charge");
            return Ok(());
        }
        loop {
            let Some(line) = read_line_capped(&mut stream, MAX_LINE_BYTES).await? else {
                return Ok(());
            };
            if line.trim().is_empty() {
                continue;
            }
            let started = Instant::now();
            let req: Request = match serde_json::from_str(&line) {
                Ok(req) => req,
                Err(err) => {
                    write_line(
                        &mut stream,
                        &Response::failure("", "protocol.invalid_json", err.to_string()),
                    )
                    .await?;
                    continue;
                }
            };
            let id = req.id.clone();
            let verb = req.verb.clone();
            let args = Value::Object(req.args.clone());
            let ok = if guest_verb_allowed(&req.verb) {
                let (ok, fd) = self.handle_and_write(req, &mut stream).await?;
                debug_assert!(fd.is_none(), "guest dispatch cannot produce fds");
                ok
            } else {
                write_line(
                    &mut stream,
                    &Response::failure(
                        id.clone(),
                        "verb.denied",
                        format!("verb {verb} is not available on the guest channel"),
                    ),
                )
                .await?;
                false
            };
            info!(
                id = %id,
                verb = %verb,
                args = %args,
                caller_transport = "vsock",
                caller_service = %name,
                ok,
                duration_ms = started.elapsed().as_millis() as u64,
                "audit"
            );
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
                self.guests.update_report(name, frame.hello.as_ref(), report);
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

/// Verbs a guest may issue over `_1024`. Everything in the machine-plane
/// contract except the socket broker: fds must never flow guestward.
fn guest_verb_allowed(verb: &Verb) -> bool {
    !matches!(verb, Verb::GuestListener | Verb::GuestConnect)
}

async fn write_line(stream: &mut UnixStream, response: &Response) -> Result<()> {
    stream
        .write_all((serde_json::to_string(response)? + "\n").as_bytes())
        .await?;
    Ok(())
}
