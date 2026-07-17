//! Shared protocol for the Hearth agent plane (docs/agent-plane.md).
//!
//! Everything both sides of an agent-plane wire must agree on lives here: the
//! protocol version, the vsock port map, hello/report frames, the AG-UI event
//! vocabulary, the task model, signed task references, and the small transport
//! utilities (hybrid-vsock CONNECT handshake, SCM_RIGHTS fd passing) that the
//! host daemons and guestd share. This crate is to the agent plane what
//! `hearth-proto` is to the machine plane.

pub mod b64;
pub mod events;
pub mod fdpass;
pub mod hmac;
pub mod hybrid;
pub mod task;
pub mod taskref;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

/// Version of the agent-plane wire protocol. Sent in every hello; a mismatch is
/// a clean error, never a guess (the workaround #9 rule, applied from day 1).
pub const AGENT_PROTOCOL_VERSION: u32 = 2;

/// Longest accepted line on any agent-plane line-JSON channel. Guests are
/// assumed adversarial; a reader must fail a connection that exceeds this
/// rather than buffer without bound.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Vsock port map (§6). Guest→host ports land on `<vm>.sock_<port>` unix
/// sockets under the hybrid model; 1027 is an in-guest listener reached via
/// `CONNECT 1027` on `<vm>.sock`.
pub const PORT_VERBS: u32 = 1024; // guest→host: hearthd verb channel (agent-in-charge)
pub const PORT_REPORT: u32 = 1025; // guest→host: boot report / readiness / heartbeat
pub const PORT_AGENT: u32 = 1026; // guest→host: MCP frames + guestd upcalls (agentd)
pub const PORT_GUESTD: u32 = 1027; // host→guest: task verbs, attach, inject.turn

/// First frame on every agent-plane channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u32,
    pub component: String,
    pub version: String,
    /// Selects the multiplexed channel on port 1026.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<HelloChannel>,
    /// For `channel: "mcp"`: the CLI session this shim serves (§2.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

impl Hello {
    pub fn new(component: &str, version: &str) -> Self {
        Self {
            proto: AGENT_PROTOCOL_VERSION,
            component: component.to_string(),
            version: version.to_string(),
            channel: None,
            thread_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelloChannel {
    Mcp,
    Upcall,
}

/// Frames guestd sends on the boot-report channel (port 1025). The first frame
/// carries `hello` + `report`; later frames re-report on change or heartbeat.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuestFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello: Option<Hello>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<BootReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<Heartbeat>,
}

/// hearthd's reply to every report frame. `restored: true` tells guestd the
/// boot (or reconnect) follows a `restore`, so it must rotate task
/// incarnations (§3.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportAck {
    pub restored: bool,
}

/// Frames hearthd sends on the boot-report channel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<ReportAck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub ts: String,
}

/// In-guest readiness and telemetry (§2.1). Reported addresses are
/// corroborating telemetry only — hearthd's lease-based resolution stays the
/// routing truth.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootReport {
    pub ready: bool,
    #[serde(default)]
    pub addrs: Vec<String>,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub agents: Vec<AgentDecl>,
    #[serde(default)]
    pub boot_id: String,
}

/// One agent CLI guestd can (or refuses to) adapt. `ok: false` with an error is
/// the loud boot-report refusal for an unpinned CLI version (§2.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDecl {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Agent-plane verbs, spoken on `/run/hearth/agent.sock` (hearthctl → agentd)
/// and on the guestd task channel (agentd → guestd, port 1027). Same
/// `Request`/`Response` framing as the machine plane.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentVerb {
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "version")]
    Version,
    #[serde(rename = "agent-ls")]
    AgentLs,
    #[serde(rename = "task.start")]
    TaskStart,
    #[serde(rename = "task.status")]
    TaskStatus,
    #[serde(rename = "task.events")]
    TaskEvents,
    #[serde(rename = "task.attach")]
    TaskAttach,
    #[serde(rename = "task.respond")]
    TaskRespond,
    #[serde(rename = "task.followup")]
    TaskFollowup,
    #[serde(rename = "task.cancel")]
    TaskCancel,
    #[serde(rename = "task.list")]
    TaskList,
    #[serde(rename = "task.gc")]
    TaskGc,
    #[serde(rename = "inject.turn")]
    InjectTurn,
}

impl AgentVerb {
    pub const ALL: &'static [AgentVerb] = &[
        AgentVerb::Ping,
        AgentVerb::Version,
        AgentVerb::AgentLs,
        AgentVerb::TaskStart,
        AgentVerb::TaskStatus,
        AgentVerb::TaskEvents,
        AgentVerb::TaskAttach,
        AgentVerb::TaskRespond,
        AgentVerb::TaskFollowup,
        AgentVerb::TaskCancel,
        AgentVerb::TaskList,
        AgentVerb::TaskGc,
        AgentVerb::InjectTurn,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Version => "version",
            Self::AgentLs => "agent-ls",
            Self::TaskStart => "task.start",
            Self::TaskStatus => "task.status",
            Self::TaskEvents => "task.events",
            Self::TaskAttach => "task.attach",
            Self::TaskRespond => "task.respond",
            Self::TaskFollowup => "task.followup",
            Self::TaskCancel => "task.cancel",
            Self::TaskList => "task.list",
            Self::TaskGc => "task.gc",
            Self::InjectTurn => "inject.turn",
        }
    }
}

impl fmt::Display for AgentVerb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Agent-plane request. Mirrors `hearth_proto::Request` with the agent verb
/// set; responses reuse `hearth_proto::Response` verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    pub id: String,
    pub verb: AgentVerb,
    #[serde(default)]
    pub args: Map<String, Value>,
}

impl AgentRequest {
    pub fn new(id: impl Into<String>, verb: AgentVerb, args: Map<String, Value>) -> Self {
        Self {
            id: id.into(),
            verb,
            args,
        }
    }
}

/// MCP tools agentd serves for agent-to-agent delegation (§7.1).
pub const MCP_TOOLS: &[&str] = &[
    "list_agents",
    "delegate",
    "wait_for",
    "task_events",
    "task_respond",
    "task_status",
    "task_cancel",
];

pub fn agent_version_result(crate_version: &str) -> Value {
    serde_json::json!({
        "protocol": AGENT_PROTOCOL_VERSION,
        "version": crate_version,
        "verbs": AgentVerb::ALL.iter().map(|verb| verb.as_str()).collect::<Vec<_>>(),
        "mcp_tools": MCP_TOOLS,
    })
}

/// Read one `\n`-terminated line without buffering past it and without letting
/// an adversarial peer grow the buffer unbounded. Returns `None` on clean EOF
/// before any byte. Byte-at-a-time is deliberate: every caller hands the same
/// stream to a different protocol layer right after the line it read.
pub async fn read_line_capped<R>(reader: &mut R, max: usize) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        if buf.len() >= max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeds cap",
            ));
        }
        buf.push(byte[0]);
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "line is not utf-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_verb_all_is_exhaustive_with_unique_wire_names() {
        fn witness(verb: &AgentVerb) {
            match verb {
                AgentVerb::Ping
                | AgentVerb::Version
                | AgentVerb::AgentLs
                | AgentVerb::TaskStart
                | AgentVerb::TaskStatus
                | AgentVerb::TaskEvents
                | AgentVerb::TaskAttach
                | AgentVerb::TaskRespond
                | AgentVerb::TaskFollowup
                | AgentVerb::TaskCancel
                | AgentVerb::TaskList
                | AgentVerb::TaskGc
                | AgentVerb::InjectTurn => {}
            }
        }
        for verb in AgentVerb::ALL {
            witness(verb);
        }
        assert_eq!(AgentVerb::ALL.len(), 13);
        let mut names: Vec<&str> = AgentVerb::ALL.iter().map(|verb| verb.as_str()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total);
    }

    #[test]
    fn agent_verbs_round_trip_their_dotted_wire_names() {
        for verb in AgentVerb::ALL {
            let wire = serde_json::to_string(verb).unwrap();
            assert_eq!(wire, format!("\"{}\"", verb.as_str()));
            let parsed: AgentVerb = serde_json::from_str(&wire).unwrap();
            assert_eq!(&parsed, verb);
        }
    }

    #[test]
    fn boot_report_frame_matches_spec_shape() {
        let frame: GuestFrame = serde_json::from_str(
            r#"{"hello": {"proto": 2, "component": "guestd", "version": "0.1.0"},
                "report": {"ready": true, "addrs": ["192.168.122.31/24"],
                           "hostname": "web-a",
                           "agents": [{"name": "codex", "ok": true}],
                           "boot_id": "b1"}}"#,
        )
        .unwrap();
        let report = frame.report.unwrap();
        assert!(report.ready);
        assert_eq!(report.agents[0].name, "codex");
        assert_eq!(frame.hello.unwrap().proto, AGENT_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn read_line_capped_reads_lines_and_rejects_floods() {
        let mut input = std::io::Cursor::new(b"hello\r\nworld\n".to_vec());
        assert_eq!(
            read_line_capped(&mut input, 64).await.unwrap().as_deref(),
            Some("hello")
        );
        assert_eq!(
            read_line_capped(&mut input, 64).await.unwrap().as_deref(),
            Some("world")
        );
        assert!(read_line_capped(&mut input, 64).await.unwrap().is_none());

        let mut flood = std::io::Cursor::new(vec![b'x'; 1024]);
        assert!(read_line_capped(&mut flood, 16).await.is_err());
    }
}
