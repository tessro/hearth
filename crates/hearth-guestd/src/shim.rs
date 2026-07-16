//! The MCP shim (docs/agent-plane.md §2.4): `hearth-guestd mcp --thread <id>`,
//! a stdio subcommand the CLIs launch as a local MCP server. It is a dumb
//! frame pipe in the literal sense — MCP's stdio JSON-RPC framing flows
//! unmodified between the CLI's stdio and agentd's MCP server over one vsock
//! connection (port 1026, hello frame `channel: "mcp"`). No HTTP anywhere on
//! this path (§13.7).
//!
//! The shim templates nothing and interprets nothing: it writes one hello line
//! carrying its `thread_id`, then splices stdin↔host and host↔stdout byte for
//! byte. A guest can therefore only ever mislabel its *own* threads (§2.4).

use crate::transport::Transport;
use anyhow::{Context, Result};
use hearth_agent_proto::{Hello, HelloChannel, PORT_AGENT};
use tokio::io::{AsyncWriteExt, copy};

pub async fn run(transport: Transport, thread_id: &str) -> Result<()> {
    let stream = transport
        .dial_host(PORT_AGENT)
        .await
        .context("dial agentd MCP port")?;
    let (host_read, mut host_write) = tokio::io::split(stream);

    // The one and only frame the shim authors: a hello selecting the MCP
    // channel and naming this session's thread.
    let mut hello = Hello::new("mcp-shim", env!("CARGO_PKG_VERSION"));
    hello.channel = Some(HelloChannel::Mcp);
    hello.thread_id = Some(thread_id.to_string());
    host_write
        .write_all((serde_json::to_string(&hello)? + "\n").as_bytes())
        .await?;
    host_write.flush().await?;

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut host_read = host_read;

    // Splice both directions until either side closes. Everything after the
    // hello is opaque JSON-RPC framing the shim never parses.
    let to_host = async {
        let _ = copy(&mut stdin, &mut host_write).await;
        let _ = host_write.shutdown().await;
    };
    let to_cli = async {
        let _ = copy(&mut host_read, &mut stdout).await;
        let _ = stdout.flush().await;
    };
    tokio::join!(to_host, to_cli);
    Ok(())
}
