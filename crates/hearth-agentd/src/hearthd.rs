//! Client for hearthd's machine-plane socket: discovery (`agent-endpoints`)
//! and the socket broker (`guest-listener`, `guest-connect`) with SCM_RIGHTS
//! fd receipt (docs/agent-plane.md §6). agentd never opens paths in the vsock
//! directory itself; hearthd binds/connects and passes the fd.

use anyhow::{anyhow, bail, Context, Result};
use camino::Utf8Path;
use hearth_agent_proto::{
    fdpass, hybrid, read_line_capped, AgentDecl, Hello, AGENT_PROTOCOL_VERSION, PORT_AGENT,
    PORT_GUESTD,
};
use hearth_proto::{Request, Response, Verb};
use serde_json::{json, Map, Value};
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use ulid::Ulid;

const LINE_CAP: usize = 1024 * 1024;

pub struct Hearthd {
    socket: camino::Utf8PathBuf,
}

impl Hearthd {
    pub fn new(socket: &Utf8Path) -> Self {
        Self {
            socket: socket.to_owned(),
        }
    }

    async fn connect(&self) -> Result<UnixStream> {
        UnixStream::connect(self.socket.as_str())
            .await
            .with_context(|| format!("connect hearthd {}", self.socket))
    }

    /// A plain request/response round-trip.
    async fn call(&self, verb: Verb, args: Map<String, Value>) -> Result<Value> {
        let mut stream = self.connect().await?;
        let req = Request::new(Ulid::new().to_string(), verb, args);
        write_request(&mut stream, &req).await?;
        let resp = read_response(&mut stream).await?;
        if resp.ok {
            Ok(resp.result.unwrap_or_else(|| json!({})))
        } else {
            let err = resp.error.unwrap_or_else(|| hearth_proto::ErrorBody {
                code: "unknown".into(),
                message: "no error body".into(),
            });
            bail!("{}: {}", err.code, err.message)
        }
    }

    /// The agent-enabled VMs and their guestd telemetry (§4.2 discovery).
    pub async fn agent_endpoints(&self) -> Result<Vec<AgentEndpoint>> {
        let value = self.call(Verb::AgentEndpoints, Map::new()).await?;
        let agents = value
            .get("agents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(agents
            .into_iter()
            .filter_map(|entry| {
                Some(AgentEndpoint {
                    id: entry.get("id")?.as_str()?.to_string(),
                    hostname: entry.get("hostname")?.as_str()?.to_string(),
                    running: entry
                        .get("running")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    is_agent_in_charge: entry
                        .get("is_agent_in_charge")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    ready: entry
                        .get("guestd")
                        .and_then(|g| g.get("ready"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    agents: entry
                        .get("guestd")
                        .and_then(|g| g.get("agents"))
                        .cloned()
                        .and_then(|agents| serde_json::from_value(agents).ok())
                        .unwrap_or_default(),
                })
            })
            .collect())
    }

    pub async fn set_hostname(&self, id: &str, hostname: &str) -> Result<Value> {
        self.call(
            Verb::Rename,
            args(&[("id", json!(id)), ("hostname", json!(hostname))]),
        )
        .await
    }

    /// Broker: bind `<id>.sock_1026` and receive the listening fd, wrapped as a
    /// tokio listener that receives guestd upcalls + MCP frames.
    pub async fn guest_listener(&self, id: &str) -> Result<tokio::net::UnixListener> {
        let mut stream = self.connect().await?;
        let req = Request::new(
            Ulid::new().to_string(),
            Verb::GuestListener,
            args(&[("id", json!(id)), ("port", json!(PORT_AGENT))]),
        );
        write_request(&mut stream, &req).await?;
        let resp = read_response(&mut stream).await?;
        if !resp.ok {
            let err = resp.error.map(|e| format!("{}: {}", e.code, e.message));
            bail!("guest-listener refused: {}", err.unwrap_or_default());
        }
        let fd = fdpass::recv_fd(&stream)
            .await
            .context("receive listener fd")?;
        let std_listener =
            unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd.into_raw_fd()) };
        std_listener.set_nonblocking(true)?;
        tokio::net::UnixListener::from_std(std_listener).context("adopt brokered listener")
    }

    /// Broker: connect `<vm>.sock` (CHV hybrid vsock), receive the connected
    /// fd, and perform the in-band `CONNECT 1027` handshake so the returned
    /// stream speaks directly to that guest's task-verb server.
    pub async fn guest_connect(&self, id: &str) -> Result<UnixStream> {
        let mut stream = self.connect().await?;
        let req = Request::new(
            Ulid::new().to_string(),
            Verb::GuestConnect,
            args(&[("id", json!(id))]),
        );
        write_request(&mut stream, &req).await?;
        let resp = read_response(&mut stream).await?;
        if !resp.ok {
            let err = resp.error.map(|e| format!("{}: {}", e.code, e.message));
            bail!("guest-connect refused: {}", err.unwrap_or_default());
        }
        let fd: OwnedFd = fdpass::recv_fd(&stream).await.context("receive guest fd")?;
        let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd.into_raw_fd()) };
        std_stream.set_nonblocking(true)?;
        let mut guest = UnixStream::from_std(std_stream).context("adopt brokered guest stream")?;
        hybrid::connect_handshake(&mut guest, PORT_GUESTD)
            .await
            .context("CONNECT 1027 handshake")?;
        let hello = Hello::new("agentd", env!("CARGO_PKG_VERSION"));
        guest
            .write_all((serde_json::to_string(&hello)? + "\n").as_bytes())
            .await
            .context("send guestd hello")?;
        let response = read_response(&mut guest)
            .await
            .context("read guestd hello response")?;
        if !response.ok {
            let err = response
                .error
                .map(|err| format!("{}: {}", err.code, err.message))
                .unwrap_or_else(|| "guestd rejected hello without an error body".to_string());
            bail!("guestd hello refused: {err}");
        }
        let peer_proto = response
            .result
            .as_ref()
            .and_then(|result| result.get("proto"))
            .and_then(Value::as_u64);
        if peer_proto != Some(u64::from(AGENT_PROTOCOL_VERSION)) {
            bail!(
                "guestd protocol mismatch: expected {}, got {:?}",
                AGENT_PROTOCOL_VERSION,
                peer_proto
            );
        }
        Ok(guest)
    }
}

#[derive(Debug, Clone)]
pub struct AgentEndpoint {
    pub id: String,
    pub hostname: String,
    pub running: bool,
    pub is_agent_in_charge: bool,
    pub ready: bool,
    pub agents: Vec<AgentDecl>,
}

fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

async fn write_request(stream: &mut UnixStream, req: &Request) -> Result<()> {
    stream
        .write_all((serde_json::to_string(req)? + "\n").as_bytes())
        .await?;
    Ok(())
}

async fn read_response(stream: &mut UnixStream) -> Result<Response> {
    let line = read_line_capped(stream, LINE_CAP)
        .await?
        .ok_or_else(|| anyhow!("hearthd closed without a response"))?;
    serde_json::from_str(&line).context("parse hearthd response")
}
