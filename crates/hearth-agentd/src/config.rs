//! hearth-agentd configuration (docs/agent-plane.md §4). Secrets (the HTTP
//! bearer token, the task-ref HMAC key) arrive via files that systemd
//! `LoadCredential=` populates — never baked into argv.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "hearth-agentd",
    version,
    about = "Hearth agent-plane host daemon"
)]
pub struct Config {
    /// hearthd's control socket (for agent-endpoints + the socket broker).
    #[arg(long, env = "HEARTH_SOCKET", default_value = "/run/hearth.sock")]
    pub hearthd_socket: Utf8PathBuf,
    /// agentd's own control socket, spoken by `hearthctl agent`.
    #[arg(
        long,
        env = "HEARTH_AGENT_SOCKET",
        default_value = "/run/hearth/agent.sock"
    )]
    pub control_socket: Utf8PathBuf,
    /// Append-only delegation ledger directory (§4.4).
    #[arg(
        long,
        env = "HEARTH_AGENT_LEDGER",
        default_value = "/var/lib/hearth-agentd/ledger"
    )]
    pub ledger_dir: Utf8PathBuf,
    /// AG-UI HTTP bind address. Never `0.0.0.0` silently; empty disables HTTP.
    #[arg(long, env = "HEARTH_AGENT_HTTP_BIND", default_value = "127.0.0.1:8787")]
    pub http_bind: String,
    /// File holding the HTTP bearer token (via `LoadCredential=`). Empty file
    /// or missing path with HTTP enabled is a hard error.
    #[arg(long, env = "HEARTH_AGENT_TOKEN_FILE")]
    pub token_file: Option<Utf8PathBuf>,
    /// File holding the current task-ref HMAC key.
    #[arg(long, env = "HEARTH_AGENT_REF_KEY_FILE")]
    pub ref_key_file: Option<Utf8PathBuf>,
    /// File holding the previous task-ref HMAC key (accepted during rotation).
    #[arg(long, env = "HEARTH_AGENT_REF_KEY_PREV_FILE")]
    pub ref_key_prev_file: Option<Utf8PathBuf>,
    /// CORS origin allowlist (comma-separated); empty means no browser origin
    /// is allowed.
    #[arg(long, env = "HEARTH_AGENT_CORS_ORIGINS", default_value = "")]
    pub cors_origins: String,
    /// Delegation allowlist: fixed VM ids permitted to delegate (§7.3).
    #[arg(long, env = "HEARTH_AGENT_DELEGATORS", default_value = "")]
    pub delegators: String,
    /// Signed-ref lifetime, seconds.
    #[arg(long, env = "HEARTH_AGENT_REF_TTL", default_value_t = 86_400)]
    pub ref_ttl_secs: i64,
    /// Disable the HTTP leg (headless deployments / tests).
    #[arg(long, env = "HEARTH_AGENT_NO_HTTP", default_value_t = false)]
    pub no_http: bool,
}

impl Config {
    pub fn cors_list(&self) -> Vec<String> {
        split_csv(&self.cors_origins)
    }

    pub fn delegator_list(&self) -> Vec<String> {
        split_csv(&self.delegators)
    }
}

fn split_csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Read a secret file, trimming a trailing newline. Returns `None` for an
/// unset path; errors for a set-but-unreadable path (a misconfiguration must
/// not silently disable auth).
pub async fn read_secret(path: &Option<Utf8PathBuf>) -> Result<Option<Vec<u8>>> {
    match path {
        None => Ok(None),
        Some(path) => {
            let mut bytes = tokio::fs::read(path.as_std_path())
                .await
                .with_context(|| format!("read secret {path}"))?;
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            Ok(Some(bytes))
        }
    }
}
