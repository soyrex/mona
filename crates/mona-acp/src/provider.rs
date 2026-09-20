//! Provider construction from `Auth`.
//!
//! Phase 3.5: this module implements a **stub** `Provider` that captures
//! auth state and exposes the upstream `Provider` trait surface. The stub
//! does NOT make network calls and cannot drive a real model turn — for
//! that, the real provider constructors from `mona-provider-openai-runtime`,
//! `mona-provider-anthropic-runtime`, etc. are wired in Phase 4 / a
//! follow-up session.
//!
//! The stub exists today because:
//!
//! 1. It proves the `Auth → Arc<dyn Provider>` round-trip works end-to-end.
//! 2. The next Phase can swap the stub for the real constructors without
//!    changing the `session/new` flow.
//! 3. Tests can assert "session X has a Provider Y" without needing a
//!    live model API.
//!
//! When the real provider construction lands, the only thing that
//! changes is the body of `build_provider_for_session`. The
//! `ProviderHandle` returned to `Session` stays the same.

use crate::auth::Auth;
use crate::provider_whitelist::{SupportedProvider, parse_provider};
use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use mona_message_types::{Message, StreamEvent, ToolDefinition};
use mona_provider_core::{EventStream, ModelRoute, Provider};
use std::sync::Arc;


/// A handle to the (stub or real) Provider for a session. Holds an
/// optional reference to the Auth that created it. `auth = None` means
/// "no credential is configured for this provider" — the stub still
/// loads but `provider_name` surfaces as `null` and the session cannot
/// drive a model turn until auth is added.
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
            .field("auth", &self.auth.as_ref().map(|a| a.masked_summary()))
            .field("provider_name", &self.provider.name())
            .field("model", &self.provider.model())
            .finish()
    }
}

/// Build a `ProviderHandle` for a session, given the auth registry.
///
/// In Phase 3.5 this returns a stub. Phase 4 swaps in real provider
/// construction (`OpenAIProvider::new(creds)`, `AnthropicProvider::new()`,
/// etc.).
///
/// If no auth is configured for the requested provider, we still build
/// the stub (so `session/new` succeeds and downstream session lifecycle
/// methods keep working) but `auth` is `None` and the handle's
/// `provider_name` surfaces as `null` to the ACP client. Sessions in
/// this state are useful for read-only surfaces (session/list,
/// session/cancel) but cannot drive a model turn.
pub fn build_provider_for_session(
    provider_str: &str,
    auth_registry: &crate::auth::AuthRegistry,
) -> Result<ProviderHandle> {
    let kind = parse_provider(provider_str).map_err(|e| anyhow::anyhow!("{e}"))?;
    let auth = auth_registry.get(kind).cloned();

    let provider: Arc<dyn Provider> = Arc::new(AuthStubProvider::new(
        kind,
        auth.as_ref(),
    ));

    Ok(ProviderHandle {
        provider,
        auth,
        provider_kind: kind,
    })
}

/// Stub Provider implementation. Captures auth state, returns the
/// configured model's metadata, but errors on any actual completion.
///
/// When `complete` is called, the stub returns an error explaining that
/// real provider construction has not landed yet. Phase 4 will replace
/// this with the real constructors.
#[derive(Clone)]
pub struct AuthStubProvider {
    kind: SupportedProvider,
    model: String,
    auth_summary: String,
}

impl AuthStubProvider {
    pub fn new(kind: SupportedProvider, auth: Option<&Auth>) -> Self {
        Self {
            kind,
            model: kind.default_model().to_string(),
            auth_summary: match auth {
                Some(a) => a.masked_summary(),
                None => "no auth configured".to_string(),
            },
        }
    }
}

#[async_trait]
impl Provider for AuthStubProvider {
    fn name(&self) -> &str {
        match self.kind {
            SupportedProvider::Codex => "codex-stub",
            SupportedProvider::Claude => "claude-stub",
            SupportedProvider::Minimax => "minimax-stub",
        }
    }

    fn display_name(&self) -> String {
        format!("{} (Phase 3.5 stub)", self.kind.as_str())
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    async fn prewarm(&self, _tools: &[ToolDefinition], _system_static: &str) {
        // No-op for the stub. Real providers may warm HTTP/2 connections,
        // refresh catalogs, etc.
    }

    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        // Phase 3.5: this is the gap. The real provider constructors
        // (OpenAIProvider, AnthropicProvider, etc.) need to be wired in
        // for `complete` to actually drive a model. The Phase 3.5 stub
        // returns a single error event so the surrounding ACP plumbing
        // can be exercised without a live provider.
        let error_msg = format!(
            "phase3.5_stub_no_real_provider: mona-acp Phase 3.5 stub: \
             provider `{}` ({}) is configured but real provider construction \
             has not landed. Phase 4 will wire `mona-provider-openai-runtime` \
             / `mona-provider-anthropic-runtime`.",
            self.kind.as_str(),
            self.auth_summary
        );
        let error_event = mona_message_types::StreamEvent::Error {
            message: error_msg,
            retry_after_secs: None,
        };
        let stream = stream::once(async move { Ok(error_event) });
        Ok(Box::pin(stream))
    }

    async fn complete_split(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        _system_static: &str,
        _system_dynamic: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        self.complete(messages, tools, "", resume_session_id).await
    }

    fn available_models_display(&self) -> Vec<String> {
        vec![self.model.clone()]
    }

    fn model_routes(&self) -> Vec<ModelRoute> {
        vec![ModelRoute {
            model: self.model.clone(),
            provider: self.kind.as_str().to_string(),
            api_method: format!("{}-stub", self.kind.as_str()),
            available: true,
            detail: format!("Phase 3.5 stub for {} ({})", self.kind.as_str(), self.auth_summary),
            usage: None,
            cheapness: None,
        }]
    }

    fn active_auth_method_label(&self) -> Option<&'static str> {
        Some("stub (Phase 3.5)")
    }

    fn transport(&self) -> Option<String> {
        Some("none (Phase 3.5 stub)".into())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        // Phase 3.5 stub has no mutable state, so the forked handle can
        // just be another `Arc<AuthStubProvider>`. Phase 4 will need to
        // clone whatever mutable state the real provider holds.
        Arc::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Auth, AuthRegistry};
    use crate::provider_whitelist::SupportedProvider;

    fn registry_with_codex() -> AuthRegistry {
        let mut r = AuthRegistry::default();
        r.inner_mut().insert(
            SupportedProvider::Codex,
            Auth::OpenaiApiKey {
                api_key: "sk-test-1234".into(),
            },
        );
        r
    }

    #[test]
    fn builds_handle_when_auth_configured() {
        let r = registry_with_codex();
        let handle = build_provider_for_session("codex", &r).unwrap();
        assert_eq!(handle.provider_kind, SupportedProvider::Codex);
        assert_eq!(handle.provider.name(), "codex-stub");
        assert_eq!(handle.provider.model(), "gpt-5.5");
        assert!(handle.auth.is_some());
        assert!(handle
            .auth
            .as_ref()
            .unwrap()
            .masked_summary()
            .contains("OpenAI"));
    }

    #[test]
    fn builds_placeholder_handle_when_no_auth() {
        // Phase 3.5 contract: missing auth is not an error. We still
        // build a stub handle so session lifecycle methods work; the
        // handle just has `auth: None` and the stub refuses `complete`.
        let r = AuthRegistry::default();
        let handle = build_provider_for_session("codex", &r).unwrap();
        assert_eq!(handle.provider_kind, SupportedProvider::Codex);
        assert_eq!(handle.provider.name(), "codex-stub");
        assert!(handle.auth.is_none());
    }

    #[test]
    fn rejects_unknown_provider() {
        let r = AuthRegistry::default();
        let err = build_provider_for_session("gemini", &r).unwrap_err();
        assert!(err.to_string().contains("gemini"));
        assert!(err.to_string().contains("Supported providers"));
    }

    #[test]
    fn stub_provider_reports_kind_in_name() {
        let r = registry_with_codex();
        let handle = build_provider_for_session("codex", &r).unwrap();
        assert!(handle.provider.name().contains("stub"));
        assert!(handle.provider.display_name().contains("Phase 3.5"));
    }

    #[test]
    fn stub_provider_listed_model_is_default_for_kind() {
        let r = registry_with_codex();
        let handle = build_provider_for_session("codex", &r).unwrap();
        let routes = handle.provider.model_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].model, "gpt-5.5");
        assert_eq!(routes[0].provider, "codex");
    }
}
