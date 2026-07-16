//! hearth-guestd entry point. Two modes:
//!
//! - default: the daemon — boot report, task registry, task-verb server.
//! - `mcp --thread <id>`: the dumb stdio↔vsock MCP shim (§2.4) the CLIs launch.

use anyhow::Result;
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use hearth_guestd::transport::parse_transport;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "hearth-guestd", version, about = "In-guest Hearth agent-plane daemon")]
struct Cli {
    /// Transport: `vsock` (real AF_VSOCK inside a VM) or `unix` (CHV
    /// hybrid-model emulation for tests / hypervisor-less dev). Global so it
    /// works whether it precedes or follows the `mcp` subcommand.
    #[arg(long, global = true, env = "HEARTH_GUESTD_TRANSPORT", default_value = "vsock")]
    transport: String,
    /// Emulated hybrid-vsock directory (unix transport only).
    #[arg(long, global = true, env = "HEARTH_GUESTD_UNIX_DIR")]
    unix_dir: Option<Utf8PathBuf>,
    /// This VM's service name (unix transport only; identity is the socket
    /// path in production).
    #[arg(long, global = true, env = "HEARTH_GUESTD_VM")]
    vm: Option<String>,
    /// Durable task registry root.
    #[arg(
        long,
        env = "HEARTH_GUESTD_STATE",
        default_value = "/var/lib/hearth-guestd"
    )]
    state_dir: Utf8PathBuf,
    /// The codex binary the adapter drives (overridable for tests).
    #[arg(long, env = "HEARTH_GUESTD_CODEX", default_value = "codex")]
    codex_command: String,
    /// The claude binary the adapter drives (Phase 5). Absent → no claude
    /// adapter is registered.
    #[arg(long, env = "HEARTH_GUESTD_CLAUDE")]
    claude_command: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// The MCP shim: splice CLI stdio ↔ agentd over vsock for one thread.
    Mcp {
        #[arg(long)]
        thread: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();
    let transport = parse_transport(&cli.transport, &cli.unix_dir, &cli.vm)?;

    if let Some(Command::Mcp { thread }) = &cli.command {
        return hearth_guestd::shim::run(transport, thread).await;
    }

    let engine = hearth_guestd::build_engine_with(
        &cli.state_dir,
        hearth_guestd::AdapterConfig {
            codex_command: cli.codex_command.clone(),
            claude_command: cli.claude_command.clone(),
        },
    )?;
    let hostname = hostname();
    let addrs = local_addrs();
    let boot_id = boot_id();
    hearth_guestd::serve(transport, engine, boot_id, hostname, addrs).await
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Corroborating telemetry only (§2.1): the lease is routing truth. Best-effort
/// — an empty list is fine.
fn local_addrs() -> Vec<String> {
    Vec::new()
}
