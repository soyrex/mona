//! Session lifecycle — `session/new`, `session/resume`, `session/cancel`,
//! `session/list`. Real session state in Phase 2; full `session/prompt`
//! driving `Agent::run_turn` lands in Phase 2.5.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::auth::AuthRegistry;
use crate::provider::{ProviderHandle, build_provider_with_model};
use crate::provider_whitelist::{SupportedProvider, parse_provider};

/// One ACP session. Holds a `ProviderHandle` for the per-session
/// provider + auth state (Phase 3.5+).
#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub provider: SupportedProvider,
    pub model: String,
    pub effort: String,
    pub working_dir: Option<String>,
    pub created_at: i64,
    /// Real provider when authenticated; unavailable placeholder otherwise.
    pub handle: Option<ProviderHandle>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("working_dir", &self.working_dir)
            .field("created_at", &self.created_at)
            .field("handle", &self.handle)
            .finish()
    }
}

impl Session {
    fn new(
        provider: SupportedProvider,
        model: String,
        effort: String,
        working_dir: Option<String>,
        handle: Option<ProviderHandle>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            provider,
            model,
            effort,
            working_dir,
            created_at: chrono::Utc::now().timestamp_millis(),
            handle,
        }
    }
}

/// Shared session registry. Held behind a Mutex by the server loop.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<Mutex<HashMap<String, Session>>>,
}

impl SessionRegistry {
    pub fn new_session(
        &self,
        provider_str: &str,
        model: Option<&str>,
        effort: Option<&str>,
        working_dir: Option<String>,
        auth: &AuthRegistry,
    ) -> Result<Session, SessionError> {
        let provider = parse_provider(provider_str)?;
        let handle = build_provider_with_model(
            provider_str,
            auth,
            model.unwrap_or(provider.default_model()),
        )
        .map_err(|e| SessionError::ProviderBuild(e.to_string()))?;
        let session = Session::new(
            provider,
            model
                .map(|s| s.to_string())
                .unwrap_or_else(|| provider.default_model().to_string()),
            effort
                .map(|s| s.to_string())
                .unwrap_or_else(|| provider.default_effort().to_string()),
            working_dir,
            Some(handle),
        );
        self.inner
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<Session> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    pub fn cancel(&self, id: &str) -> bool {
        self.inner.lock().unwrap().remove(id).is_some()
    }

    /// Update the model + effort for an existing session. Phase 2.5 calls
    /// this after the per-turn router decides on a new model.
    pub fn update(&self, session: &Session) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if let Some(existing) = inner.get_mut(&session.id) {
            existing.model = session.model.clone();
            existing.effort = session.effort.clone();
            existing.working_dir = session.working_dir.clone();
            // Don't clobber the handle — it was set at new_session time.
            true
        } else {
            false
        }
    }

    pub fn list(&self) -> Vec<Session> {
        self.inner.lock().unwrap().values().cloned().collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Provider(#[from] crate::provider_whitelist::ProviderError),
    #[error("provider construction failed: {0}")]
    ProviderBuild(String),
}

/// JSON-RPC result payload for `session/new` and `session/resume`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub provider_name: Option<String>,
}

impl From<&Session> for SessionInfo {
    fn from(s: &Session) -> Self {
        // `provider_name` is `Some(<runtime-id>)` only when the session's
        // handle was built from a real auth record. Sessions created
        // without auth (Phase 3.5 placeholder handles) surface
        // `provider_name: null` so clients can distinguish "configured"
        // from "placeholder" sessions.
        let provider_name = s.handle.as_ref().and_then(|h| {
            if h.auth.is_some() {
                Some(h.provider.name().to_string())
            } else {
                None
            }
        });
        Self {
            session_id: s.id.clone(),
            provider: s.provider.as_str().to_string(),
            model: s.model.clone(),
            effort: s.effort.clone(),
            provider_name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with_all() -> AuthRegistry {
        let mut r = AuthRegistry::default();
        r.inner_mut().insert(
            SupportedProvider::Codex,
            crate::auth::Auth::OpenaiApiKey {
                api_key: "sk-test-codex-1234".into(),
            },
        );
        r.inner_mut().insert(
            SupportedProvider::Claude,
            crate::auth::Auth::AnthropicApiKey {
                api_key: "sk-ant-test-claude-1234".into(),
            },
        );
        r.inner_mut().insert(
            SupportedProvider::Minimax,
            crate::auth::Auth::MinimaxApiKey {
                api_key: "minimax-test-minimax-1234".into(),
                api_base: "https://api.minimax.io/v1".into(),
            },
        );
        r
    }

    #[test]
    fn new_session_attaches_provider_handle_when_auth_configured() {
        let r = SessionRegistry::default();
        let auth = registry_with_all();
        let s = r.new_session("codex", None, None, None, &auth).unwrap();
        assert!(s.handle.is_some());
        let h = s.handle.unwrap();
        assert_eq!(h.provider_kind, SupportedProvider::Codex);
        assert!(h.auth.is_some(), "auth should be Some when configured");
    }

    #[test]
    fn new_session_creates_placeholder_handle_when_auth_missing() {
        // Phase 3.5: missing auth does NOT fail session/new. Instead, a
        // placeholder handle is attached with `auth: None` so the session
        // can be listed/cancelled but `provider_name` surfaces as null
        // and completion returns an authentication error.
        let r = SessionRegistry::default();
        let auth = AuthRegistry::default();
        let s = r.new_session("codex", None, None, None, &auth).unwrap();
        assert!(s.handle.is_some());
        assert!(s.handle.as_ref().unwrap().auth.is_none());
    }

    #[test]
    fn session_info_provider_name_null_when_unconfigured() {
        let r = SessionRegistry::default();
        let auth = AuthRegistry::default();
        let s = r.new_session("codex", None, None, None, &auth).unwrap();
        let info: SessionInfo = (&s).into();
        assert_eq!(info.provider_name, None);
    }

    #[test]
    fn session_info_provider_name_some_when_configured() {
        let r = SessionRegistry::default();
        let auth = registry_with_all();
        let s = r.new_session("codex", None, None, None, &auth).unwrap();
        let info: SessionInfo = (&s).into();
        assert_eq!(info.provider_name.as_deref(), Some("openai"));
    }

    #[test]
    fn new_session_uses_provider_defaults() {
        let r = SessionRegistry::default();
        let auth = registry_with_all();
        let s = r.new_session("codex", None, None, None, &auth).unwrap();
        assert_eq!(s.provider, SupportedProvider::Codex);
        assert_eq!(s.model, "gpt-5.5");
        assert_eq!(s.effort, "high");
        assert!(r.get(&s.id).is_some());
    }

    #[test]
    fn configured_session_preserves_explicit_model_in_runtime() {
        let registry = SessionRegistry::default();
        let session = registry
            .new_session("codex", Some("gpt-5.4"), None, None, &registry_with_all())
            .unwrap();
        assert_eq!(session.model, "gpt-5.4");
        assert_eq!(session.handle.unwrap().provider.model(), "gpt-5.4");
    }

    #[test]
    fn new_session_rejects_unsupported_provider() {
        let r = SessionRegistry::default();
        let auth = AuthRegistry::default();
        let err = r
            .new_session("gemini", None, None, None, &auth)
            .unwrap_err();
        match err {
            SessionError::Provider(_) => {}
            _ => panic!("expected Provider error, got {err:?}"),
        }
    }

    #[test]
    fn cancel_removes_session() {
        let r = SessionRegistry::default();
        let auth = registry_with_all();
        let s = r.new_session("claude", None, None, None, &auth).unwrap();
        assert!(r.cancel(&s.id));
        assert!(r.get(&s.id).is_none());
        assert!(!r.cancel(&s.id));
    }

    #[test]
    fn list_returns_all_sessions() {
        let r = SessionRegistry::default();
        let auth = registry_with_all();
        r.new_session("codex", None, None, None, &auth).unwrap();
        r.new_session("claude", None, None, None, &auth).unwrap();
        r.new_session("minimax", None, None, None, &auth).unwrap();
        assert_eq!(r.list().len(), 3);
    }
}
