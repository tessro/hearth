use anyhow::Result;
use clap::Parser;
use hearthd::{config::Config, ensure_dirs, host::RealHost, reconcile, Daemon};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::parse();
    init_tracing();
    ensure_dirs(&cfg).await?;
    let host = RealHost;
    reconcile(&cfg, &host).await?;
    Daemon::new(cfg, host).serve().await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt::layer();
    let registry = tracing_subscriber::registry().with(filter).with(fmt);
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            registry.with(journald).init();
            return;
        }
    }
    registry.init();
}
