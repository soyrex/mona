//! Session lifecycle — `session/new`, `session/resume`, `session/cancel`,
//! `session/list`. Real session state in Phase 2; full `session/prompt`
//! driving `Agent::run_turn` lands in Phase 2.5.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::provider_whitelist::{SupportedProvider, parse_provider};

/// One ACP session. Currently a metadata record; will gain Agent state
/// in Phase 2.5.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub provider: SupportedProvider,
    pub model: String,
    pub effort: String,
    pub working_dir: Option<String>,
    pub created_at: i64,
}

impl Session {
    fn new(provider: SupportedProvider, model: String, effort: String, working_dir: Option<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            provider,
            model,
            effort,
            working_dir,
            created_at: chrono::Utc::now().timestamp_millis(),
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
    ) -> Result<Session, SessionError> {
        let provider = parse_provider(provider_str)?;
        let session = Session::new(
            provider,
            model.unwrap_or(provider.default_model()).to_string(),
            effort.unwrap_or(provider.default_effort()).to_string(),
            working_dir,
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
}

/// JSON-RPC result payload for `session/new` and `session/resume`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub effort: String,
}

impl From<&Session> for SessionInfo {
    fn from(s: &Session) -> Self {
        Self {
            session_id: s.id.clone(),
            provider: s.provider.as_str().to_string(),
            model: s.model.clone(),
            effort: s.effort.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_uses_provider_defaults() {
        let r = SessionRegistry::default();
        let s = r.new_session("codex", None, None, None).unwrap();
        assert_eq!(s.provider, SupportedProvider::Codex);
        assert_eq!(s.model, "gpt-5.5");
        assert_eq!(s.effort, "high");
        assert!(r.get(&s.id).is_some());
    }

    #[test]
    fn new_session_rejects_unsupported_provider() {
        let r = SessionRegistry::default();
        let err = r.new_session("gemini", None, None, None).unwrap_err();
        match err {
            SessionError::Provider(_) => {}
        }
    }

    #[test]
    fn cancel_removes_session() {
        let r = SessionRegistry::default();
        let s = r.new_session("claude", None, None, None).unwrap();
        assert!(r.cancel(&s.id));
        assert!(r.get(&s.id).is_none());
        // Cancelling again is a no-op and returns false.
        assert!(!r.cancel(&s.id));
    }

    #[test]
    fn list_returns_all_sessions() {
        let r = SessionRegistry::default();
        r.new_session("codex", None, None, None).unwrap();
        r.new_session("claude", None, None, None).unwrap();
        r.new_session("minimax", None, None, None).unwrap();
        assert_eq!(r.list().len(), 3);
    }
}
