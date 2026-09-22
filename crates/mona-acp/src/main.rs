//! `mona-acp` binary entry point. Reads ACP JSON-RPC frames from stdin and
//! writes responses to stdout.

use anyhow::Result;
use mona_acp::{ClassifierStartup, ServerState, classifier_from_environment};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// The dependency graph currently enables both Rustls crypto backends:
/// reqwest 0.12 selects ring while Azure's reqwest 0.13 path selects AWS-LC.
/// Rustls cannot infer a process default in that configuration, so select the
/// workspace's declared AWS-LC provider before any HTTP or WebSocket client is
/// constructed.
fn install_rustls_crypto_provider() -> Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .map_err(|_| anyhow::anyhow!("could not install the AWS-LC Rustls crypto provider"))?;
    }
    Ok(())
}

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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    install_rustls_crypto_provider()?;

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

    let (classifier, classifier_startup) = classifier_from_environment(&home);
    match classifier_startup {
        ClassifierStartup::Live => tracing::info!("live Jev ACP classifier enabled"),
        ClassifierStartup::RuleBasedDisabled => tracing::info!(
            "using offline rule-based classifier; set MONA_ACP_LIVE_JEV=1 to opt into live Jev ACP routing or 0 to force offline routing"
        ),
        ClassifierStartup::UnavailableInvalidOptIn => tracing::warn!(
            "invalid live Jev activation/configuration; routing will report unavailable and retain the current model, without heuristic fallback"
        ),
        ClassifierStartup::UnavailableLiveJev => tracing::warn!(
            "live Jev ACP opt-in has no usable credential configuration; routing will report unavailable and retain the current model, without heuristic fallback"
        ),
    }
    let state = ServerState::new(home.clone(), classifier);
    tracing::info!(?home, "mona-acp starting");
    mona_acp::run_acp_server(state).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn installs_crypto_provider_before_tls_client_construction() {
        super::install_rustls_crypto_provider().expect("install Rustls crypto provider");
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        reqwest::Client::builder()
            .build()
            .expect("construct TLS client after provider installation");
    }
}
