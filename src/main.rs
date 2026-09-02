mod agent_process;
mod auth_terminal;
mod bridge;
mod elicitation_validation;
mod filesystem;
mod mcp;
mod mcp_config;
mod options;
mod runtime_cache;
mod runtime_state;
mod semantic;
mod server;
mod terminal;

use anyhow::Result;
use clap::Parser;
use options::Options;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("attyd=info")),
        )
        .with_target(false)
        .init();

    server::serve(Options::parse()).await
}
