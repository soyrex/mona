//! Real provider construction with credentials owned by the ACP session.
//! Construction performs no model call. Prompt/agent-loop wiring is separate.

use crate::auth::{Auth, AuthRegistry};
use crate::provider_whitelist::{SupportedProvider, parse_provider};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use mona_base::auth::codex::CodexCredentials;
use mona_message_types::{Message, ToolDefinition};
use mona_provider_anthropic_runtime::{AnthropicCredentials, AnthropicProvider};
use mona_provider_core::{EventStream, Provider};
use mona_provider_openai_runtime::OpenAIProvider;
use mona_provider_openrouter_runtime::OpenRouterProvider;
use std::sync::Arc;

#[derive(Clone)]
pub struct ProviderHandle {
    pub provider: Arc<dyn Provider>,
    pub auth: Option<Auth>,
    pub provider_kind: SupportedProvider,
}

impl std::fmt::Debug for ProviderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderHandle")
            .field("provider_kind", &self.provider_kind)
            .field("configured", &self.auth.is_some())
            .field("provider_name", &self.provider.name())
            .field("model", &self.provider.model())
            .finish()
    }
}

pub fn build_provider_for_session(
    provider_str: &str,
    auth_registry: &AuthRegistry,
) -> Result<ProviderHandle> {
    let kind = parse_provider(provider_str)?;
    build_provider_with_model(provider_str, auth_registry, kind.default_model())
}

pub(crate) fn build_provider_with_model(
    provider_str: &str,
    auth_registry: &AuthRegistry,
    model: &str,
) -> Result<ProviderHandle> {
    let kind = parse_provider(provider_str)?;
    ensure!(!model.trim().is_empty(), "model must not be empty");
    let auth = auth_registry.get(kind).cloned();
    if let Some(auth) = &auth {
        ensure!(
            auth.provider() == kind,
            "credential does not match requested provider"
        );
    }
    let provider: Arc<dyn Provider> = match &auth {
        Some(Auth::OpenaiApiKey { api_key }) => {
            ensure!(
                !api_key.trim().is_empty(),
                "OpenAI API key must not be empty"
            );
            Arc::new(OpenAIProvider::new_with_credentials_and_model(
                CodexCredentials {
                    access_token: api_key.clone(),
                    refresh_token: String::new(),
                    id_token: None,
                    account_id: None,
                    expires_at: None,
                },
                model,
            ))
        }
        Some(Auth::OpenaiOauth {
            access_token,
            refresh_token,
            expires_at_ms,
            account_id,
        }) => {
            ensure!(
                !access_token.trim().is_empty() && !refresh_token.trim().is_empty(),
                "OpenAI OAuth requires access and refresh tokens"
            );
            Arc::new(OpenAIProvider::new_with_credentials_and_model(
                CodexCredentials {
                    access_token: access_token.clone(),
                    refresh_token: refresh_token.clone(),
                    id_token: None,
                    account_id: account_id.clone(),
                    expires_at: (*expires_at_ms > 0).then_some(*expires_at_ms),
                },
                model,
            ))
        }
        Some(Auth::AnthropicApiKey { api_key }) => Arc::new(AnthropicProvider::with_credentials(
            model,
            AnthropicCredentials::ApiKey(api_key.clone()),
        )?),
        Some(Auth::AnthropicOauth {
            access_token,
            refresh_token,
            expires_at_ms,
        }) => Arc::new(AnthropicProvider::with_credentials(
            model,
            AnthropicCredentials::OAuth {
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
                expires_at_ms: *expires_at_ms,
            },
        )?),
        Some(Auth::MinimaxApiKey { api_key, api_base }) => {
            Arc::new(OpenRouterProvider::new_minimax_with_credentials(
                api_key.clone(),
                api_base.clone(),
                model,
            )?)
        }
        None => Arc::new(UnconfiguredProvider {
            kind,
            model: model.to_owned(),
        }),
    };
    Ok(ProviderHandle {
        provider,
        auth,
        provider_kind: kind,
    })
}

/// Missing auth preserves session/list/cancel behavior, but never pretends to
/// be an authenticated provider or falls back to ambient credentials.
#[derive(Clone)]
struct UnconfiguredProvider {
    kind: SupportedProvider,
    model: String,
}

#[async_trait]
impl Provider for UnconfiguredProvider {
    fn name(&self) -> &str {
        "unconfigured"
    }
    fn model(&self) -> String {
        self.model.clone()
    }
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        anyhow::bail!("no auth configured for {}", self.kind.as_str())
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(auth: Auth) -> AuthRegistry {
        let mut registry = AuthRegistry::default();
        registry.inner_mut().insert(auth.provider(), auth);
        registry
    }

    #[test]
    fn constructs_real_runtimes_for_all_auth_variants() {
        for (auth, name) in [
            (
                Auth::OpenaiApiKey {
                    api_key: "test-openai-key".into(),
                },
                "openai",
            ),
            (
                Auth::OpenaiOauth {
                    access_token: "test-access".into(),
                    refresh_token: "test-refresh".into(),
                    expires_at_ms: i64::MAX,
                    account_id: Some("test-account".into()),
                },
                "openai",
            ),
            (
                Auth::AnthropicApiKey {
                    api_key: "test-anthropic-key".into(),
                },
                "anthropic",
            ),
            (
                Auth::AnthropicOauth {
                    access_token: "test-access".into(),
                    refresh_token: "test-refresh".into(),
                    expires_at_ms: i64::MAX,
                },
                "anthropic",
            ),
            (
                Auth::MinimaxApiKey {
                    api_key: "test-minimax-key".into(),
                    api_base: "https://api.minimax.io/v1".into(),
                },
                "minimax",
            ),
        ] {
            let kind = auth.provider();
            let handle = build_provider_for_session(kind.as_str(), &registry(auth)).unwrap();
            assert_eq!(handle.provider.name(), name);
            assert_eq!(handle.provider.model(), kind.default_model());
            assert_eq!(handle.provider.fork().model(), kind.default_model());
            assert!(handle.auth.is_some());
        }
    }

    #[tokio::test]
    async fn unconfigured_provider_refuses_completion() {
        for kind in SupportedProvider::all() {
            let handle =
                build_provider_for_session(kind.as_str(), &AuthRegistry::default()).unwrap();
            assert!(handle.auth.is_none());
            assert!(handle.provider.complete(&[], &[], "", None).await.is_err());
        }
    }

    #[test]
    fn rejects_invalid_provider_and_credentials() {
        assert!(build_provider_for_session("gemini", &AuthRegistry::default()).is_err());
        assert!(
            build_provider_for_session(
                "codex",
                &registry(Auth::OpenaiApiKey {
                    api_key: " ".into()
                })
            )
            .is_err()
        );
        let mut mismatched = AuthRegistry::default();
        mismatched.inner_mut().insert(
            SupportedProvider::Codex,
            Auth::AnthropicApiKey {
                api_key: "secret".into(),
            },
        );
        assert!(build_provider_for_session("codex", &mismatched).is_err());
    }
}
