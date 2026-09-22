mod agent_process;
mod auth_terminal;
mod auto_close;
mod bridge;
mod completion_handoff;
mod dev_proxy;
mod elicitation_validation;
mod event_queue;
mod filesystem;
mod history_cache;
mod history_replay;
mod mcp;
mod mcp_config;
mod options;
mod ordered_ingress;
mod runtime_cache;
mod runtime_state;
mod semantic;
mod server;
mod session_dispatch;
mod session_mirror;
mod session_observation;
mod session_observers;
mod session_presentation;
mod session_registry;
mod session_resources;
mod session_state;
mod terminal;
#[cfg(test)]
mod test_allocations;
mod websocket_agent;

use anyhow::Result;
use clap::Parser;
use options::Options;
use tracing_subscriber::EnvFilter;

const VERSION: &str = match option_env!("ATTYD_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

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
