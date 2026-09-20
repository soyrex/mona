//! Minimal auth loader for mona-acp.
//!
//! Reads provider credentials from `~/.mona/<provider>.json`.
//!
//! Phase 3 deliberately uses a small, well-defined JSON shape instead of
//! pulling in upstream's auth crate. Upstream's `mona-base::auth::*` is
//! tightly coupled to the daemon lifecycle, the macOS Keychain, and a
//! notification broker that we don't need in mona-acp.
//!
//! Phase 4 (or a focused Phase 3-follow-up) can replace this with a real
//! upstream integration. For now, the JSON shape is:
//!
//! ```json
//! // ~/.mona/codex.json (OpenAI)
//! { "kind": "openai_oauth", "access_token": "...", "refresh_token": "...", "expires_at_ms": 0 }
//! // or
//! { "kind": "openai_api_key", "api_key": "sk-..." }
//!
//! // ~/.mona/claude.json (Anthropic)
//! { "kind": "anthropic_oauth", "access_token": "...", "refresh_token": "...", "expires_at_ms": 0 }
//! // or
//! { "kind": "anthropic_api_key", "api_key": "sk-ant-..." }
//!
//! // ~/.mona/minimax.json (MiniMax)
//! { "kind": "minimax_api_key", "api_key": "...", "api_base": "https://api.minimax.io/v1" }
//! ```
//!
//! If no file is present, the loader falls back to env vars:
//! `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `MINIMAX_API_KEY`.
//!
//! If neither file nor env var exists, the provider is "not configured"
//! and `Agent::run_turn` cannot drive a real model call. The session is
//! still created (so the routing infrastructure can be tested end-to-end
//! without a real provider).

use crate::provider_whitelist::SupportedProvider;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Auth state for one provider. `None` means "not configured".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Auth {
    OpenaiOauth {
        access_token: String,
        refresh_token: String,
        #[serde(default)]
        expires_at_ms: i64,
        #[serde(default)]
        account_id: Option<String>,
    },
    OpenaiApiKey {
        api_key: String,
    },
    AnthropicOauth {
        access_token: String,
        refresh_token: String,
        #[serde(default)]
        expires_at_ms: i64,
    },
    AnthropicApiKey {
        api_key: String,
    },
    MinimaxApiKey {
        api_key: String,
        #[serde(default = "default_minimax_api_base")]
        api_base: String,
    },
}

fn default_minimax_api_base() -> String {
    "https://api.minimax.io/v1".to_string()
}

impl Auth {
    /// Provider this auth applies to.
    pub fn provider(&self) -> SupportedProvider {
        match self {
            Auth::OpenaiOauth { .. } | Auth::OpenaiApiKey { .. } => SupportedProvider::Codex,
            Auth::AnthropicOauth { .. } | Auth::AnthropicApiKey { .. } => SupportedProvider::Claude,
            Auth::MinimaxApiKey { .. } => SupportedProvider::Minimax,
        }
    }

    /// Mask the credential for safe logging.
    pub fn masked_summary(&self) -> String {
        match self {
            Auth::OpenaiOauth { access_token, .. } => {
                format!("OpenAI OAuth ({})", mask_token(access_token))
            }
            Auth::OpenaiApiKey { api_key } => {
                format!("OpenAI API key ({})", mask_token(api_key))
            }
            Auth::AnthropicOauth { access_token, .. } => {
                format!("Anthropic OAuth ({})", mask_token(access_token))
            }
            Auth::AnthropicApiKey { api_key } => {
                format!("Anthropic API key ({})", mask_token(api_key))
            }
            Auth::MinimaxApiKey { api_key, .. } => {
                format!("MiniMax API key ({})", mask_token(api_key))
            }
        }
    }
}

fn mask_token(token: &str) -> String {
    if token.len() <= 8 {
        "***".into()
    } else {
        let head = &token[..4];
        let tail = &token[token.len() - 4..];
        format!("{head}…{tail}")
    }
}

/// Auth registry. Holds loaded `Auth` records keyed by provider.
#[derive(Debug, Default, Clone)]
pub struct AuthRegistry {
    inner: std::collections::HashMap<SupportedProvider, Auth>,
}

impl AuthRegistry {
    /// Load auth for all Phase 2 providers from `<home_dir>/<provider>.json`
    /// or env-var fallback. Logs a warning for each provider that is not
    /// configured; the loader is permissive — missing auth is not an error.
    pub fn load(home_dir: &Path) -> Self {
        let mut inner = std::collections::HashMap::new();

        for provider in SupportedProvider::all() {
            match Self::load_one(home_dir, *provider) {
                Ok(Some(auth)) => {
                    tracing::info!(
                        provider = %provider.as_str(),
                        auth = %auth.masked_summary(),
                        "loaded provider auth"
                    );
                    inner.insert(*provider, auth);
                }
                Ok(None) => {
                    tracing::warn!(
                        provider = %provider.as_str(),
                        "no auth configured; provider will not be usable until auth is set up"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %provider.as_str(),
                        error = %e,
                        "failed to load provider auth; ignoring"
                    );
                }
            }
        }

        Self { inner }
    }

    fn load_one(home_dir: &Path, provider: SupportedProvider) -> Result<Option<Auth>> {
        // Try the home-dir file first.
        let path = auth_path(home_dir, provider);
        if path.exists() {
            let s = std::fs::read_to_string(&path)
                .with_context(|| format!("read auth file {}", path.display()))?;
            let auth: Auth = serde_json::from_str(&s)
                .with_context(|| format!("parse auth file {}", path.display()))?;
            if auth.provider() != provider {
                anyhow::bail!(
                    "auth file {} declares {:?} but expected {:?}",
                    path.display(),
                    auth.provider(),
                    provider
                );
            }
            return Ok(Some(auth));
        }

        // Fall back to env vars.
        let env_var = match provider {
            SupportedProvider::Codex => "OPENAI_API_KEY",
            SupportedProvider::Claude => "ANTHROPIC_API_KEY",
            SupportedProvider::Minimax => "MINIMAX_API_KEY",
        };
        if let Ok(api_key) = std::env::var(env_var) {
            let auth = match provider {
                SupportedProvider::Codex => Auth::OpenaiApiKey { api_key },
                SupportedProvider::Claude => Auth::AnthropicApiKey { api_key },
                SupportedProvider::Minimax => Auth::MinimaxApiKey {
                    api_key,
                    api_base: default_minimax_api_base(),
                },
            };
            return Ok(Some(auth));
        }

        Ok(None)
    }

    /// Returns `true` if the provider has configured auth.
    pub fn has_auth(&self, provider: SupportedProvider) -> bool {
        self.inner.contains_key(&provider)
    }

    /// Get the auth record for a provider.
    pub fn get(&self, provider: SupportedProvider) -> Option<&Auth> {
        self.inner.get(&provider)
    }

    /// List providers that have configured auth.
    pub fn configured_providers(&self) -> Vec<SupportedProvider> {
        let mut v: Vec<_> = self.inner.keys().copied().collect();
        v.sort_by_key(|p| p.as_str());
        v
    }

    /// Mutable accessor to the inner map. Used by tests; production code
    /// should call `load()` instead.
    #[cfg(test)]
    pub fn inner_mut(&mut self) -> &mut std::collections::HashMap<SupportedProvider, Auth> {
        &mut self.inner
    }
}

/// Path to the auth file for a provider: `<home_dir>/<provider>.json`.
pub fn auth_path(home_dir: &Path, provider: SupportedProvider) -> PathBuf {
    home_dir.join(format!("{}.json", provider.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_token_handles_short_and_long() {
        assert_eq!(mask_token("sk"), "***");
        assert_eq!(mask_token("sk-1234567890abcdef"), "sk-1…cdef");
    }

    #[test]
    fn auth_provider_matches_variant() {
        assert_eq!(
            Auth::OpenaiApiKey { api_key: "k".into() }.provider(),
            SupportedProvider::Codex
        );
        assert_eq!(
            Auth::AnthropicApiKey { api_key: "k".into() }.provider(),
            SupportedProvider::Claude
        );
        assert_eq!(
            Auth::MinimaxApiKey {
                api_key: "k".into(),
                api_base: default_minimax_api_base()
            }
            .provider(),
            SupportedProvider::Minimax
        );
    }

    #[test]
    fn load_returns_empty_registry_when_no_files_or_env() {
        let tmp = std::env::temp_dir().join(format!("mona-auth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let registry = AuthRegistry::load(&tmp);
        assert!(registry.configured_providers().is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_reads_api_key_file() {
        let tmp = std::env::temp_dir().join(format!("mona-auth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("codex.json"),
            r#"{"kind":"openai_api_key","api_key":"sk-test-1234"}"#,
        )
        .unwrap();
        let registry = AuthRegistry::load(&tmp);
        assert!(registry.has_auth(SupportedProvider::Codex));
        assert!(!registry.has_auth(SupportedProvider::Claude));
        assert_eq!(
            registry.configured_providers(),
            vec![SupportedProvider::Codex]
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_rejects_wrong_provider_for_file() {
        let tmp = std::env::temp_dir().join(format!("mona-auth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        // Put a Claude auth inside the codex.json file — should be rejected.
        std::fs::write(
            tmp.join("codex.json"),
            r#"{"kind":"anthropic_api_key","api_key":"sk-test"}"#,
        )
        .unwrap();
        let registry = AuthRegistry::load(&tmp);
        // The bad file is logged and skipped; registry is empty.
        assert!(!registry.has_auth(SupportedProvider::Codex));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn auth_path_matches_provider_name() {
        let tmp = std::path::PathBuf::from("/tmp");
        assert_eq!(auth_path(&tmp, SupportedProvider::Codex), PathBuf::from("/tmp/codex.json"));
        assert_eq!(auth_path(&tmp, SupportedProvider::Claude), PathBuf::from("/tmp/claude.json"));
        assert_eq!(auth_path(&tmp, SupportedProvider::Minimax), PathBuf::from("/tmp/minimax.json"));
    }
}
