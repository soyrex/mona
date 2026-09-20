//! `mona-acp` binary entry point. Reads ACP JSON-RPC frames from stdin and
//! writes responses to stdout.

use anyhow::Result;
use mona_acp::ServerState;
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MONA_ACP_LOG")
                .unwrap_or_else(|_| EnvFilter::new("mona_acp=info,warn")),
        )
        .with_target(false)
        .init();

    let state = ServerState::new();
    mona_acp::run_acp_server(state).await
}
