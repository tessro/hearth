//! Guest-side transport: real AF_VSOCK inside a VM, or a byte-identical unix
//! emulation of CHV's hybrid model for tests and hypervisor-less development.
//!
//! In `vsock` mode, guest→host dials `AF_VSOCK (2, port)` and host→guest
//! arrives on an in-guest `AF_VSOCK` listener (port 1027) — CHV performs the
//! `CONNECT`/`OK` handshake on the host side and splices the raw stream.
//!
//! In `unix` mode, guestd plays CHV's role itself: guest→host dials
//! `<dir>/<vm>.sock_<port>` directly (exactly where CHV would land the bytes),
//! and guestd binds `<dir>/<vm>.sock`, answering the same `CONNECT <port>`
//! handshake hearthd/agentd would send a real CHV socket. Host-side code
//! cannot tell the difference — that is the point.

use crate::vsock_io::{VsockListener, VsockStream, VMADDR_CID_HOST};
use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;
use hearth_agent_proto::{hybrid, PORT_GUESTD};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{UnixListener, UnixStream};
use tracing::warn;

pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

pub type DynStream = Box<dyn Stream>;

#[derive(Debug, Clone)]
pub enum Transport {
    Vsock,
    Unix { dir: Utf8PathBuf, vm: String },
}

impl Transport {
    /// Dial a host-side port (guest → host).
    pub async fn dial_host(&self, port: u32) -> Result<DynStream> {
        match self {
            Transport::Vsock => {
                let stream = VsockStream::connect(VMADDR_CID_HOST, port)
                    .await
                    .with_context(|| format!("connect vsock host port {port}"))?;
                Ok(Box::new(stream))
            }
            Transport::Unix { dir, vm } => {
                let path = dir.join(format!("{vm}.sock_{port}"));
                let stream = UnixStream::connect(path.as_str())
                    .await
                    .with_context(|| format!("connect emulated host port {path}"))?;
                Ok(Box::new(stream))
            }
        }
    }

    /// Serve the guestd port (host → guest, port 1027): yields one stream per
    /// accepted connection through `on_conn`.
    pub async fn serve_guest_port<F>(&self, on_conn: F) -> Result<()>
    where
        F: Fn(DynStream) + Send + Sync + 'static,
    {
        match self {
            Transport::Vsock => {
                let listener =
                    VsockListener::bind(PORT_GUESTD).context("bind in-guest vsock port 1027")?;
                loop {
                    match listener.accept().await {
                        Ok(stream) => on_conn(Box::new(stream)),
                        Err(err) => {
                            warn!(error = %err, "vsock accept failed");
                        }
                    }
                }
            }
            Transport::Unix { dir, vm } => {
                let path = dir.join(format!("{vm}.sock"));
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err).context("remove stale emulated vsock socket"),
                }
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let listener = UnixListener::bind(path.as_str())
                    .with_context(|| format!("bind emulated vsock socket {path}"))?;
                loop {
                    match listener.accept().await {
                        Ok((mut stream, _)) => {
                            // Play CHV: answer the hybrid CONNECT handshake.
                            match hybrid::accept_handshake(&mut stream).await {
                                Ok(port) if port == PORT_GUESTD => on_conn(Box::new(stream)),
                                Ok(port) => {
                                    warn!(port, "emulated CONNECT to a port nobody serves");
                                }
                                Err(err) => {
                                    warn!(error = %err, "emulated CONNECT handshake failed");
                                }
                            }
                        }
                        Err(err) => {
                            warn!(error = %err, "emulated vsock accept failed");
                        }
                    }
                }
            }
        }
    }
}

pub fn parse_transport(
    mode: &str,
    dir: &Option<Utf8PathBuf>,
    vm: &Option<String>,
) -> Result<Transport> {
    match mode {
        "vsock" => Ok(Transport::Vsock),
        "unix" => match (dir, vm) {
            (Some(dir), Some(vm)) => Ok(Transport::Unix {
                dir: dir.clone(),
                vm: vm.clone(),
            }),
            _ => bail!("--transport unix requires --unix-dir and --vm"),
        },
        other => bail!("unknown transport {other:?} (vsock|unix)"),
    }
}
