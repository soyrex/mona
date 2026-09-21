//! `mona-acp` binary entry point. Reads ACP JSON-RPC frames from stdin and
//! writes responses to stdout.

use anyhow::Result;
use mona_acp::ServerState;
use mona_jev::{JevClassifier, RuleBasedClassifier};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

/// Resolve the mona home directory.
///
/// Default: `~/.mona/`. Override with `MONA_HOME` env var. Used for
/// `router-traces/` persistence and (in Phase 3) OAuth token loading.
fn home_dir() -> PathBuf {
    if let Ok(p) = std::env::var("MONA_HOME") {
        return PathBuf::from(p);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".mona");
    }
    std::env::temp_dir().join("mona")
}

/// Build the default classifier. Operators can later swap in a live
/// classifier (Phase 2.5+); for now we use the rule-based one.
fn default_classifier() -> Arc<dyn JevClassifier> {
    Arc::new(RuleBasedClassifier::new())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MONA_ACP_LOG")
                .unwrap_or_else(|_| EnvFilter::new("mona_acp=info,warn")),
        )
        .with_target(false)
        // CRITICAL: mona-acp writes JSON-RPC frames to stdout. Any log
        // line on stdout corrupts the wire stream and breaks clients
        // (Monitter's acp_runtime parser, fixture tests, our smoke).
        // Always send tracing output to stderr.
        .with_writer(std::io::stderr)
        .init();

    let home = home_dir();
    std::fs::create_dir_all(&home).ok();
    std::fs::create_dir_all(home.join("router-traces")).ok();

    let state = ServerState::new(home.clone(), default_classifier());
    tracing::info!(?home, "mona-acp starting");
    mona_acp::run_acp_server(state).await
}
