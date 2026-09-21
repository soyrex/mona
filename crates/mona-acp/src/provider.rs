//! Real provider construction with credentials owned by the ACP session.
//! Construction performs no model call. Prompt/agent-loop wiring is separate.

use crate::auth::{Auth, AuthRegistry};
use crate::provider_whitelist::{SupportedProvider, parse_provider};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use mona_base::auth::codex::CodexCredentials;
use mona_message_types::{Message, ToolDefinition};
use mona_provider_anthropic_runtime::{AnthropicCredentials, AnthropicProvider};
use mona_provider_core::{EventStream, Provider};
use mona_provider_openai_runtime::OpenAIProvider;
use mona_provider_openrouter_runtime::OpenRouterProvider;
use std::sync::Arc;
use tracing::warn;

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

impl ProviderHandle {
    /// Construct a replacement runtime from this handle's authenticated
    /// identity. The original handle is never modified: callers can swap this
    /// return value into a session only after all validation succeeds.
    pub fn reconfigure(&self, model: &str, effort: &str) -> Result<Self> {
        ensure!(
            self.auth.is_some(),
            "cannot reconfigure an unauthenticated provider"
        );
        validate_model_for_provider(self.provider_kind, model)?;
        let model = model.trim();
        let effort = effort.trim().to_ascii_lowercase();
        ensure!(!effort.is_empty(), "reasoning effort must not be empty");

        // `fork` is an independently mutable provider runtime. All fallible
        // work is deliberately performed against it, leaving the live handle
        // untouched until this method has returned successfully.
        let candidate = self.provider.fork();
        candidate.set_model(model)?;

        let available_efforts = candidate.available_efforts();
        if available_efforts.is_empty() {
            ensure!(
                matches!(effort.as_str(), "none" | "off" | "default"),
                "reasoning effort '{effort}' is unsupported by model '{model}'"
            );
            ensure!(
                candidate.reasoning_effort().is_none(),
                "provider retained reasoning effort for unsupported model '{model}'"
            );
        } else {
            candidate.set_reasoning_effort(&effort)?;
        }

        Ok(Self {
            provider: candidate,
            auth: self.auth.clone(),
            provider_kind: self.provider_kind,
        })
    }
}

/// Return the models this handle's exact credential currently exposes.
///
/// Provider runtimes also carry broad, static "known model" lists. Those are
/// useful capability metadata for Jev, but they are not proof that a specific
/// account may select a model. ACP advertising and routing therefore use the
/// authenticated provider catalogue first and only fall back to the already
/// selected model when the live catalogue cannot be refreshed. Ambient cache
/// state may belong to a different credential and is therefore not eligible.
pub async fn authenticated_available_models(handle: &ProviderHandle) -> Vec<String> {
    let live = match handle.auth.as_ref() {
        Some(Auth::OpenaiOauth { access_token, .. }) => {
            mona_base::provider::fetch_openai_model_catalog(access_token)
                .await
                .map(|catalog| catalog.available_models)
        }
        Some(Auth::OpenaiApiKey { api_key }) => {
            mona_base::provider::fetch_openai_api_key_model_catalog(api_key)
                .await
                .map(|catalog| catalog.available_models)
        }
        Some(Auth::AnthropicOauth { access_token, .. }) => {
            mona_base::provider::fetch_anthropic_model_catalog_oauth(access_token)
                .await
                .map(|catalog| catalog.available_models)
        }
        Some(Auth::AnthropicApiKey { api_key }) => {
            mona_base::provider::fetch_anthropic_model_catalog(api_key)
                .await
                .map(|catalog| catalog.available_models)
        }
        Some(Auth::MinimaxApiKey { api_key, api_base }) => {
            fetch_minimax_model_catalog(api_key, api_base).await
        }
        None => return Vec::new(),
    };

    let models = match live {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => fallback_account_models(handle),
        Err(error) => {
            warn!(
                provider = %handle.provider_kind.as_str(),
                %error,
                "authenticated model catalogue refresh failed; using safe provider fallback"
            );
            fallback_account_models(handle)
        }
    };
    filter_provider_models(handle.provider_kind, models)
}

fn fallback_account_models(handle: &ProviderHandle) -> Vec<String> {
    vec![handle.provider.model()]
}

async fn fetch_minimax_model_catalog(api_key: &str, api_base: &str) -> Result<Vec<String>> {
    let endpoint = format!("{}/models", api_base.trim_end_matches('/'));
    let response = mona_provider_core::shared_http_client()
        .get(&endpoint)
        .bearer_auth(api_key)
        .send()
        .await
        .with_context(|| format!("send MiniMax model catalogue request to {endpoint}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("read MiniMax model catalogue response from {endpoint}"))?;
    ensure!(
        status.is_success(),
        "MiniMax model catalogue request failed with {status}: {}",
        mona_base::util::truncate_str(&body, 400)
    );
    let models = parse_minimax_model_catalog(&body)?;
    Ok(limit_minimax_catalog_for_credential(
        api_key, api_base, models,
    ))
}

fn parse_minimax_model_catalog(body: &str) -> Result<Vec<String>> {
    let value: serde_json::Value =
        serde_json::from_str(body).context("parse MiniMax model catalogue JSON")?;
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .unwrap_or(&value)
        .as_array()
        .context("MiniMax model catalogue omitted its model array")?;
    Ok(entries
        .iter()
        .filter_map(|entry| {
            entry.as_str().or_else(|| {
                entry
                    .get("id")
                    .or_else(|| entry.get("name"))
                    .and_then(serde_json::Value::as_str)
            })
        })
        .map(str::to_string)
        .collect())
}

fn limit_minimax_catalog_for_credential(
    api_key: &str,
    api_base: &str,
    models: Vec<String>,
) -> Vec<String> {
    let official_minimax_api = url::Url::parse(api_base).is_ok_and(|url| {
        url.host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("api.minimax.io"))
    });
    if !official_minimax_api || !api_key.trim().starts_with("sk-cp-") {
        return models;
    }

    // MiniMax's Token Plan keys use the documented `sk-cp-` prefix and have
    // access to the current M3/M2.7 family. GET /models is a public product
    // catalogue rather than an entitlement response, so intersect it with the
    // plan's documented text-model set before advertising routes to clients.
    const TOKEN_PLAN_TEXT_MODELS: &[&str] =
        &["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed"];
    models
        .into_iter()
        .filter(|model| {
            TOKEN_PLAN_TEXT_MODELS
                .iter()
                .any(|allowed| model.eq_ignore_ascii_case(allowed))
        })
        .collect()
}

fn filter_provider_models(provider: SupportedProvider, models: Vec<String>) -> Vec<String> {
    let mut filtered = Vec::new();
    for model in models {
        let model = model.trim();
        if model.is_empty()
            || validate_model_for_provider(provider, model).is_err()
            || (provider == SupportedProvider::Codex
                && (model.eq_ignore_ascii_case(mona_provider_core::CHATGPT_WEB_MODEL)
                    || matches!(
                        model.to_ascii_lowercase().as_str(),
                        "gpt-reserve" | "codex-auto-review"
                    )))
            || filtered
                .iter()
                .any(|known: &String| known.eq_ignore_ascii_case(model))
        {
            continue;
        }
        filtered.push(model.to_string());
    }
    filtered
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

/// Keep model routing pinned to the session's authenticated provider. The ACP
/// whitelist intentionally has a small provider vocabulary, but model IDs may
/// evolve independently, so validate stable provider-family prefixes rather
/// than a frozen per-model allowlist.
fn validate_model_for_provider(kind: SupportedProvider, model: &str) -> Result<()> {
    let model = model.trim();
    ensure!(!model.is_empty(), "model must not be empty");
    ensure!(
        !model.eq_ignore_ascii_case("auto") && !model.contains(':'),
        "model must be a concrete model id"
    );
    let normalized = model.to_ascii_lowercase();
    let compatible = match kind {
        SupportedProvider::Codex => {
            normalized.starts_with("gpt-")
                || normalized.starts_with("o1")
                || normalized.starts_with("o3")
                || normalized.starts_with("o4")
                || normalized.starts_with("o5")
                || normalized.starts_with("codex")
        }
        SupportedProvider::Claude => normalized.starts_with("claude-"),
        SupportedProvider::Minimax => normalized.starts_with("minimax-"),
    };
    ensure!(
        compatible,
        "model '{model}' is incompatible with {}",
        kind.as_str()
    );
    Ok(())
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

    #[test]
    fn token_plan_catalog_excludes_paygo_only_minimax_models() {
        let parsed = parse_minimax_model_catalog(
            r#"{"data":[{"id":"MiniMax-M3"},{"id":"MiniMax-M2.7"},{"id":"MiniMax-M2.7-highspeed"},{"id":"MiniMax-M2.5"}]}"#,
        )
        .unwrap();
        let limited = limit_minimax_catalog_for_credential(
            "sk-cp-example",
            "https://api.minimax.io/v1",
            parsed,
        );
        assert_eq!(
            limited,
            vec!["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed"]
        );
    }

    #[test]
    fn custom_minimax_endpoint_keeps_its_authenticated_catalog() {
        let models = vec!["MiniMax-M3".into(), "MiniMax-M2.5".into()];
        assert_eq!(
            limit_minimax_catalog_for_credential(
                "sk-cp-relayed",
                "https://minimax.example.test/v1",
                models.clone(),
            ),
            models
        );
    }
}
