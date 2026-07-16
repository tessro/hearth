//! hearth-agentd entry point.

use anyhow::Result;
use clap::Parser;
use hearth_agentd::config::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::parse();
    init_tracing();
    let agentd = hearth_agentd::build(cfg).await?;
    hearth_agentd::run(agentd).await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry().with(filter).with(fmt).init();
}
