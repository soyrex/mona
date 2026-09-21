//! Session lifecycle — `session/new`, `session/resume`, `session/cancel`,
//! `session/list`. Durable bounded state is restored with current credentials;
//! provider execution is owned by the reviewed ACP loop.

use mona_jev::{JevMessage, JevRole, JevTurnOutcome};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::auth::AuthRegistry;
use crate::provider::{ProviderHandle, build_provider_with_model};
use crate::provider_whitelist::{SupportedProvider, parse_provider};

/// Keep enough context for the next Jev routing decision without turning the
/// session store into an unbounded transcript archive.
pub const MAX_RECENT_MESSAGES: usize = 8;
/// A single message must not make a durable record unexpectedly large.
pub const MAX_CONTEXT_MESSAGE_CHARS: usize = 8 * 1024;

/// The ACP host's per-session tool approval policy. This is deliberately
/// fail-closed: missing legacy state and unknown callers both use `Default`,
/// which continues to ask the host before a protected action.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    #[default]
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

impl PermissionMode {
    pub const fn as_acp_value(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    pub fn from_acp_value(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "bypassPermissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }
}

/// Bounded context carried across a process restart for routing only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRoutingContext {
    #[serde(default)]
    pub recent_messages: Vec<JevMessage>,
    #[serde(default)]
    pub last_turn_outcome: Option<JevTurnOutcome>,
}

#[derive(Default)]
struct RegistryState {
    sessions: HashMap<String, Session>,
    contexts: HashMap<String, SessionRoutingContext>,
}

/// On-disk representation deliberately excludes `ProviderHandle` and `Auth`.
/// Credentials are loaded only from [`AuthRegistry`] when a caller explicitly
/// resumes a session.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DurableSessionFile {
    version: u8,
    sessions: Vec<DurableSession>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DurableSession {
    id: String,
    provider: SupportedProvider,
    model: String,
    effort: String,
    working_dir: Option<String>,
    created_at: i64,
    /// Absent in pre-permission-mode durable state; serde defaults it to the
    /// safe prompt-per-tool behavior rather than silently granting access.
    #[serde(default)]
    permission_mode: PermissionMode,
    #[serde(default)]
    context: SessionRoutingContext,
}

impl From<(&Session, SessionRoutingContext)> for DurableSession {
    fn from((session, context): (&Session, SessionRoutingContext)) -> Self {
        Self {
            id: session.id.clone(),
            provider: session.provider,
            model: session.model.clone(),
            effort: session.effort.clone(),
            working_dir: session.working_dir.clone(),
            created_at: session.created_at,
            permission_mode: session.permission_mode,
            context,
        }
    }
}

impl DurableSession {
    fn into_session(self) -> (Session, SessionRoutingContext) {
        (
            Session {
                id: self.id,
                provider: self.provider,
                model: self.model,
                effort: self.effort,
                working_dir: self.working_dir,
                created_at: self.created_at,
                permission_mode: self.permission_mode,
                available_models: Vec::new(),
                // A handle is runtime-only. `resume` may rebuild one using
                // currently available auth, but reload itself never does.
                handle: None,
            },
            self.context,
        )
    }
}

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
    pub permission_mode: PermissionMode,
    /// Runtime-only, account-scoped models currently eligible for Jev. Static
    /// known-model metadata must never be copied into this list.
    pub available_models: Vec<String>,
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
            .field("permission_mode", &self.permission_mode)
            .field("available_models", &self.available_models)
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
            permission_mode: PermissionMode::Default,
            available_models: Vec::new(),
            handle,
        }
    }

    /// Atomically prepare and install a provider replacement for this already
    /// authenticated session. Construction and effort validation happen first,
    /// so an error leaves the existing handle, model, and effort unchanged.
    pub fn reconfigure_provider(&mut self, model: &str, effort: &str) -> Result<(), SessionError> {
        let replacement = self
            .handle
            .as_ref()
            .ok_or(SessionError::NoProviderHandle)?
            .reconfigure(model, effort)
            .map_err(|e| SessionError::ProviderBuild(e.to_string()))?;
        // Provider runtimes may normalize a requested value (or express a
        // disabled effort as `None`). Persist the actual live selection, not
        // an optimistic echo of the request.
        let actual_model = replacement.provider.model();
        let actual_effort = replacement
            .provider
            .reasoning_effort()
            .unwrap_or_else(|| "none".to_string());
        self.handle = Some(replacement);
        self.model = actual_model;
        self.effort = actual_effort;
        Ok(())
    }
}

/// Shared session registry. Held behind a Mutex by the server loop.
///
/// `Default` remains intentionally in-memory for existing embedders and unit
/// tests. Use [`SessionRegistry::with_state_dir`] to opt into durable session
/// metadata.
#[derive(Clone)]
pub struct SessionRegistry {
    inner: Arc<Mutex<RegistryState>>,
    state_path: Option<Arc<PathBuf>>,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryState::default())),
            state_path: None,
        }
    }
}

impl SessionRegistry {
    /// Open a registry backed by `<state_dir>/sessions.json`.
    ///
    /// A corrupt or unsupported state file fails safe: no entries are loaded
    /// and the original file is left untouched for operator inspection.
    pub fn with_state_dir(state_dir: impl AsRef<Path>) -> Result<Self, SessionError> {
        let state_dir = state_dir.as_ref();
        std::fs::create_dir_all(state_dir).map_err(|e| SessionError::Persistence {
            operation: "create state directory",
            source: e,
        })?;
        let state_path = state_dir.join("sessions.json");
        let state = Self::load_state(&state_path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(state)),
            state_path: Some(Arc::new(state_path)),
        })
    }

    /// Alias for callers that prefer a constructor-style name.
    pub fn load_from_state_dir(state_dir: impl AsRef<Path>) -> Result<Self, SessionError> {
        Self::with_state_dir(state_dir)
    }

    /// The durable state file when this registry was configured for storage.
    pub fn state_path(&self) -> Option<&Path> {
        self.state_path.as_deref().map(PathBuf::as_path)
    }

    pub fn new_session(
        &self,
        provider_str: &str,
        model: Option<&str>,
        effort: Option<&str>,
        working_dir: Option<String>,
        auth: &AuthRegistry,
    ) -> Result<Session, SessionError> {
        let provider = parse_provider(provider_str)?;
        let requested_model = model.unwrap_or(provider.default_model());
        let requested_effort = effort.unwrap_or(provider.default_effort());
        let handle = build_provider_with_model(provider_str, auth, requested_model)
            .map_err(|e| SessionError::ProviderBuild(e.to_string()))?;
        // Apply the initial effort to an authenticated runtime before storing
        // the session. Placeholder handles intentionally remain usable for
        // list/cancel without pretending to have live provider state.
        let handle = if handle.auth.is_some() {
            handle
                .reconfigure(requested_model, requested_effort)
                .map_err(|e| SessionError::ProviderBuild(e.to_string()))?
        } else {
            handle
        };
        let live_model = handle.provider.model();
        let live_effort = handle.provider.reasoning_effort();
        let session = Session::new(
            provider,
            live_model,
            live_effort.unwrap_or_else(|| requested_effort.to_string()),
            working_dir,
            Some(handle),
        );
        let mut inner = self.inner.lock().unwrap();
        let mut next = RegistryState {
            sessions: inner.sessions.clone(),
            contexts: inner.contexts.clone(),
        };
        next.sessions.insert(session.id.clone(), session.clone());
        next.contexts
            .insert(session.id.clone(), SessionRoutingContext::default());
        self.persist_state(&next)?;
        *inner = next;
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<Session> {
        self.inner.lock().unwrap().sessions.get(id).cloned()
    }

    /// Update the runtime-only, account-scoped eligibility list used by Jev.
    /// It is deliberately not persisted because provider entitlements can
    /// change between process launches.
    pub fn set_available_models(&self, id: &str, models: Vec<String>) -> Option<Session> {
        let mut inner = self.inner.lock().unwrap();
        let session = inner.sessions.get_mut(id)?;
        session.available_models = models;
        Some(session.clone())
    }

    pub fn cancel(&self, id: &str) -> bool {
        self.cancel_result(id).unwrap_or(false)
    }

    /// Remove a session and its durable metadata. The in-memory entry is only
    /// removed after the new state has been atomically committed.
    pub fn cancel_result(&self, id: &str) -> Result<bool, SessionError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.sessions.contains_key(id) {
            return Ok(false);
        }
        let mut next = RegistryState {
            sessions: inner.sessions.clone(),
            contexts: inner.contexts.clone(),
        };
        next.sessions.remove(id);
        next.contexts.remove(id);
        self.persist_state(&next)?;
        *inner = next;
        Ok(true)
    }

    /// Persist model + effort after a successful runtime update.
    pub fn update(&self, session: &Session) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.sessions.contains_key(&session.id) {
            let mut next = RegistryState {
                sessions: inner.sessions.clone(),
                contexts: inner.contexts.clone(),
            };
            let existing = next
                .sessions
                .get_mut(&session.id)
                .expect("session presence checked above");
            existing.model = session.model.clone();
            existing.effort = session.effort.clone();
            existing.working_dir = session.working_dir.clone();
            // A successfully reconfigured session carries a fresh handle.
            // Persist it with the metadata; failed reconfiguration never
            // produces a changed session to pass here.
            existing.handle = session.handle.clone();
            if self.persist_state(&next).is_ok() {
                *inner = next;
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Persist a permission-mode change atomically. The live registry is only
    /// changed once its replacement durable file has been committed.
    pub fn set_permission_mode(
        &self,
        id: &str,
        permission_mode: PermissionMode,
    ) -> Result<Session, SessionError> {
        let mut inner = self.inner.lock().unwrap();
        let mut next = RegistryState {
            sessions: inner.sessions.clone(),
            contexts: inner.contexts.clone(),
        };
        let session = next
            .sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_owned()))?;
        session.permission_mode = permission_mode;
        let result = session.clone();
        self.persist_state(&next)?;
        *inner = next;
        Ok(result)
    }

    /// Reconfigure an existing authenticated session's live provider. The
    /// stored session is mutated only after a replacement handle is ready.
    pub fn reconfigure_provider(
        &self,
        id: &str,
        model: &str,
        effort: &str,
    ) -> Result<Session, SessionError> {
        let mut inner = self.inner.lock().unwrap();
        let mut next = RegistryState {
            sessions: inner.sessions.clone(),
            contexts: inner.contexts.clone(),
        };
        let session = next
            .sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_owned()))?;
        session.reconfigure_provider(model, effort)?;
        let result = session.clone();
        self.persist_state(&next)?;
        *inner = next;
        Ok(result)
    }

    /// Atomically switch an authenticated session to a provider-qualified
    /// model. The replacement runtime is completely constructed before the
    /// durable/live session is changed, so a failed provider or model switch
    /// leaves the existing session untouched.
    pub fn switch_provider_model(
        &self,
        id: &str,
        provider: SupportedProvider,
        model: &str,
        auth: &AuthRegistry,
    ) -> Result<Session, SessionError> {
        if !auth.has_auth(provider) {
            return Err(SessionError::MissingAuth(provider));
        }
        let base = build_provider_with_model(provider.as_str(), auth, model)
            .map_err(|e| SessionError::ProviderBuild(e.to_string()))?;
        let efforts = base.provider.available_efforts();
        let requested_effort = if efforts.is_empty() {
            "none"
        } else if efforts.contains(&provider.default_effort()) {
            provider.default_effort()
        } else {
            efforts[0]
        };
        let replacement = base
            .reconfigure(model, requested_effort)
            .map_err(|e| SessionError::ProviderBuild(e.to_string()))?;
        let actual_model = replacement.provider.model();
        let actual_effort = replacement
            .provider
            .reasoning_effort()
            .unwrap_or_else(|| "none".to_string());

        let mut inner = self.inner.lock().unwrap();
        let mut next = RegistryState {
            sessions: inner.sessions.clone(),
            contexts: inner.contexts.clone(),
        };
        let session = next
            .sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_owned()))?;
        session.provider = provider;
        session.model = actual_model;
        session.effort = actual_effort;
        session.available_models.clear();
        session.handle = Some(replacement);
        let result = session.clone();
        self.persist_state(&next)?;
        *inner = next;
        Ok(result)
    }

    pub fn list(&self) -> Vec<Session> {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .values()
            .cloned()
            .collect()
    }

    /// Rebuild a live provider handle after a durable metadata reload. Auth is
    /// intentionally required here; reload never reads it from durable state.
    pub fn resume(&self, id: &str, auth: &AuthRegistry) -> Result<Session, SessionError> {
        let mut inner = self.inner.lock().unwrap();
        let session = inner
            .sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_owned()))?;
        if !auth.has_auth(session.provider) {
            return Err(SessionError::MissingAuth(session.provider));
        }
        let handle = build_provider_with_model(session.provider.as_str(), auth, &session.model)
            .map_err(|e| SessionError::ProviderBuild(e.to_string()))?
            .reconfigure(&session.model, &session.effort)
            .map_err(|e| SessionError::ProviderBuild(e.to_string()))?;
        session.handle = Some(handle);
        Ok(session.clone())
    }

    /// Read the bounded routing context belonging to a session.
    pub fn routing_context(&self, id: &str) -> Option<SessionRoutingContext> {
        self.inner.lock().unwrap().contexts.get(id).cloned()
    }

    /// Add a user, assistant, or tool message to the bounded durable context.
    pub fn record_message(
        &self,
        id: &str,
        role: JevRole,
        content: impl AsRef<str>,
    ) -> Result<(), SessionError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.sessions.contains_key(id) {
            return Err(SessionError::NotFound(id.to_owned()));
        }
        let mut next = RegistryState {
            sessions: inner.sessions.clone(),
            contexts: inner.contexts.clone(),
        };
        let context = next.contexts.entry(id.to_owned()).or_default();
        context.recent_messages.push(JevMessage {
            role,
            content: bounded_content(content.as_ref()),
        });
        trim_context(context);
        self.persist_state(&next)?;
        *inner = next;
        Ok(())
    }

    /// Persist the outcome used by the next Jev routing decision.
    pub fn set_last_turn_outcome(
        &self,
        id: &str,
        outcome: Option<JevTurnOutcome>,
    ) -> Result<(), SessionError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.sessions.contains_key(id) {
            return Err(SessionError::NotFound(id.to_owned()));
        }
        let mut next = RegistryState {
            sessions: inner.sessions.clone(),
            contexts: inner.contexts.clone(),
        };
        let context = next.contexts.entry(id.to_owned()).or_default();
        context.last_turn_outcome = outcome;
        self.persist_state(&next)?;
        *inner = next;
        Ok(())
    }

    fn load_state(state_path: &Path) -> Result<RegistryState, SessionError> {
        if !state_path.exists() {
            return Ok(RegistryState::default());
        }
        let bytes = std::fs::read(state_path).map_err(|e| SessionError::Persistence {
            operation: "read durable sessions",
            source: e,
        })?;
        let file: DurableSessionFile = serde_json::from_slice(&bytes)
            .map_err(|e| SessionError::CorruptState(e.to_string()))?;
        if file.version != 1 {
            return Err(SessionError::CorruptState(format!(
                "unsupported durable session version {}",
                file.version
            )));
        }
        let mut state = RegistryState::default();
        for durable in file.sessions {
            let (session, mut context) = durable.into_session();
            trim_context(&mut context);
            if state.sessions.contains_key(&session.id) {
                return Err(SessionError::CorruptState(format!(
                    "duplicate durable session id `{}`",
                    session.id
                )));
            }
            state.contexts.insert(session.id.clone(), context);
            state.sessions.insert(session.id.clone(), session);
        }
        Ok(state)
    }

    fn persist_state(&self, state: &RegistryState) -> Result<(), SessionError> {
        let Some(state_path) = &self.state_path else {
            return Ok(());
        };
        let mut sessions: Vec<_> = state
            .sessions
            .values()
            .map(|session| {
                DurableSession::from((
                    session,
                    state.contexts.get(&session.id).cloned().unwrap_or_default(),
                ))
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        let bytes = serde_json::to_vec_pretty(&DurableSessionFile {
            version: 1,
            sessions,
        })
        .map_err(|e| SessionError::Serialization(e.to_string()))?;

        let state_path = state_path.as_path();
        let parent = state_path.parent().expect("state file has a parent");
        let temporary = parent.join(format!(".sessions-{}.tmp", Uuid::new_v4()));
        let result = (|| -> Result<(), std::io::Error> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&temporary, state_path)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if let Err(source) = result {
            let _ = std::fs::remove_file(&temporary);
            return Err(SessionError::Persistence {
                operation: "atomically persist durable sessions",
                source,
            });
        }
        Ok(())
    }
}

fn trim_context(context: &mut SessionRoutingContext) {
    if context.recent_messages.len() > MAX_RECENT_MESSAGES {
        let excess = context.recent_messages.len() - MAX_RECENT_MESSAGES;
        context.recent_messages.drain(..excess);
    }
    for message in &mut context.recent_messages {
        message.content = bounded_content(&message.content);
    }
}

fn bounded_content(content: &str) -> String {
    let redacted = redact_sensitive_content(content);
    let mut chars = redacted.chars();
    let prefix: String = chars.by_ref().take(MAX_CONTEXT_MESSAGE_CHARS).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

/// Conversation and tool output are useful to routing, but values assigned to
/// common credential fields must never enter the durable session file. This is
/// intentionally narrow: normal prose (including discussions *about* those
/// fields) is preserved unless it uses an assignment form.
fn redact_sensitive_content(content: &str) -> String {
    content
        .split_inclusive('\n')
        .map(redact_sensitive_line)
        .collect()
}

fn redact_sensitive_line(line: &str) -> String {
    const MARKERS: [&str; 4] = ["api_key", "access_token", "refresh_token", "authorization"];
    let lower = line.to_ascii_lowercase();
    let Some((start, marker)) = MARKERS
        .iter()
        .filter_map(|marker| lower.find(marker).map(|start| (start, *marker)))
        .min_by_key(|(start, _)| *start)
    else {
        return line.to_owned();
    };
    let value_start = start + marker.len();
    let suffix = &line[value_start..];
    let trimmed = suffix.trim_start_matches([' ', '\t', ':', '=', '"', '\'']);
    if trimmed.len() == suffix.len() {
        return line.to_owned();
    }
    let prefix_len = line.len() - trimmed.len();
    let end = trimmed
        .find(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | '}'))
        .unwrap_or(trimmed.len());
    format!("{}[redacted]{}", &line[..prefix_len], &trimmed[end..])
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Provider(#[from] crate::provider_whitelist::ProviderError),
    #[error("provider construction failed: {0}")]
    ProviderBuild(String),
    #[error("session has no provider handle")]
    NoProviderHandle,
    #[error("session `{0}` not found")]
    NotFound(String),
    #[error("no configured auth is available for provider `{0:?}`")]
    MissingAuth(SupportedProvider),
    #[error("durable session state is corrupt: {0}")]
    CorruptState(String),
    #[error("could not serialize durable session state: {0}")]
    Serialization(String),
    #[error("could not {operation}: {source}")]
    Persistence {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
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
    fn reconfigure_swaps_a_fully_configured_runtime_and_update_persists_it() {
        let registry = SessionRegistry::default();
        let mut session = registry
            .new_session("codex", None, None, None, &registry_with_all())
            .unwrap();

        session.reconfigure_provider("gpt-5.4", "high").unwrap();
        assert_eq!(session.model, "gpt-5.4");
        assert_eq!(session.effort, "high");
        assert_eq!(session.handle.as_ref().unwrap().provider.model(), "gpt-5.4");
        assert_eq!(
            session
                .handle
                .as_ref()
                .unwrap()
                .provider
                .reasoning_effort()
                .as_deref(),
            Some("high")
        );

        assert!(registry.update(&session));
        let stored = registry.get(&session.id).unwrap();
        assert_eq!(stored.handle.unwrap().provider.model(), "gpt-5.4");
    }

    #[test]
    fn failed_reconfigure_keeps_the_live_handle_and_session_metadata() {
        let registry = SessionRegistry::default();
        let mut session = registry
            .new_session("codex", None, None, None, &registry_with_all())
            .unwrap();
        let original_model = session.model.clone();
        let original_effort = session.effort.clone();
        let original_handle = session.handle.as_ref().unwrap().clone();

        let err = session
            .reconfigure_provider("claude-sonnet-4-6", "high")
            .unwrap_err();
        assert!(matches!(err, SessionError::ProviderBuild(_)));
        assert_eq!(session.model, original_model);
        assert_eq!(session.effort, original_effort);
        assert!(Arc::ptr_eq(
            &session.handle.as_ref().unwrap().provider,
            &original_handle.provider
        ));
        assert_eq!(session.handle.as_ref().unwrap().provider.model(), "gpt-5.5");
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

    #[test]
    fn durable_session_round_trip_preserves_metadata_and_routing_context() {
        let temp = tempfile::tempdir().unwrap();
        let auth = registry_with_all();
        let registry = SessionRegistry::with_state_dir(temp.path()).unwrap();
        let session = registry
            .new_session(
                "codex",
                Some("gpt-5.4"),
                Some("high"),
                Some("/workspace/project".into()),
                &auth,
            )
            .unwrap();
        registry
            .record_message(&session.id, JevRole::User, "inspect this project")
            .unwrap();
        registry
            .record_message(&session.id, JevRole::Tool, "read_file completed")
            .unwrap();
        registry
            .set_last_turn_outcome(
                &session.id,
                Some(JevTurnOutcome::Uncertain {
                    reason: "tool output needs review".into(),
                }),
            )
            .unwrap();

        let reloaded = SessionRegistry::with_state_dir(temp.path()).unwrap();
        let restored = reloaded.get(&session.id).unwrap();
        assert_eq!(restored.provider, SupportedProvider::Codex);
        assert_eq!(restored.model, "gpt-5.4");
        assert_eq!(restored.effort, "high");
        assert_eq!(restored.working_dir.as_deref(), Some("/workspace/project"));
        assert!(
            restored.handle.is_none(),
            "handles are never restored from disk"
        );
        let context = reloaded.routing_context(&session.id).unwrap();
        assert_eq!(context.recent_messages.len(), 2);
        assert!(matches!(
            context.last_turn_outcome,
            Some(JevTurnOutcome::Uncertain { ref reason }) if reason == "tool output needs review"
        ));
    }

    #[test]
    fn permission_mode_is_fail_closed_and_survives_durable_reload() {
        let temp = tempfile::tempdir().unwrap();
        let registry = SessionRegistry::with_state_dir(temp.path()).unwrap();
        let session = registry
            .new_session("codex", None, None, None, &registry_with_all())
            .unwrap();
        assert_eq!(session.permission_mode, PermissionMode::Default);

        let updated = registry
            .set_permission_mode(&session.id, PermissionMode::BypassPermissions)
            .unwrap();
        assert_eq!(updated.permission_mode, PermissionMode::BypassPermissions);
        let serialized = std::fs::read_to_string(registry.state_path().unwrap()).unwrap();
        assert!(serialized.contains("\"permissionMode\": \"bypassPermissions\""));

        let reloaded = SessionRegistry::with_state_dir(temp.path()).unwrap();
        assert_eq!(
            reloaded.get(&session.id).unwrap().permission_mode,
            PermissionMode::BypassPermissions
        );
    }

    #[test]
    fn legacy_durable_session_without_permission_mode_defaults_to_ask() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("sessions.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "sessions": [{
                    "id": "legacy-session",
                    "provider": "Codex",
                    "model": "gpt-5.5",
                    "effort": "none",
                    "workingDir": null,
                    "createdAt": 1,
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        let registry = SessionRegistry::with_state_dir(temp.path()).unwrap();
        assert_eq!(
            registry.get("legacy-session").unwrap().permission_mode,
            PermissionMode::Default
        );
    }

    #[test]
    fn durable_state_does_not_serialize_auth_or_provider_handles() {
        let temp = tempfile::tempdir().unwrap();
        let registry = SessionRegistry::with_state_dir(temp.path()).unwrap();
        let auth = registry_with_all();
        let session = registry
            .new_session("codex", None, None, None, &auth)
            .unwrap();
        registry
            .record_message(&session.id, JevRole::Assistant, "safe response")
            .unwrap();
        registry
            .record_message(
                &session.id,
                JevRole::Tool,
                "api_key=tool-output-secret-that-must-not-persist",
            )
            .unwrap();

        let serialized = std::fs::read_to_string(registry.state_path().unwrap()).unwrap();
        assert!(!serialized.contains("sk-test-codex-1234"));
        assert!(!serialized.contains("sk-ant-test-claude-1234"));
        assert!(!serialized.contains("minimax-test-minimax-1234"));
        assert!(!serialized.contains("tool-output-secret-that-must-not-persist"));
        assert!(!serialized.contains("\"handle\""));
        assert!(!serialized.contains("\"auth\""));
    }

    #[test]
    fn durable_reload_requires_current_auth_to_reconstruct_a_handle() {
        let temp = tempfile::tempdir().unwrap();
        let auth = registry_with_all();
        let registry = SessionRegistry::with_state_dir(temp.path()).unwrap();
        let session = registry
            .new_session("codex", None, None, None, &auth)
            .unwrap();

        let reloaded = SessionRegistry::with_state_dir(temp.path()).unwrap();
        let error = reloaded
            .resume(&session.id, &AuthRegistry::default())
            .unwrap_err();
        assert!(matches!(
            error,
            SessionError::MissingAuth(SupportedProvider::Codex)
        ));
        assert!(reloaded.get(&session.id).unwrap().handle.is_none());

        let resumed = reloaded.resume(&session.id, &auth).unwrap();
        assert!(resumed.handle.is_some());
        assert!(resumed.handle.unwrap().auth.is_some());
    }

    #[test]
    fn durable_context_is_bounded_by_count_and_message_size() {
        let temp = tempfile::tempdir().unwrap();
        let registry = SessionRegistry::with_state_dir(temp.path()).unwrap();
        let session = registry
            .new_session("codex", None, None, None, &registry_with_all())
            .unwrap();
        for index in 0..(MAX_RECENT_MESSAGES + 3) {
            registry
                .record_message(&session.id, JevRole::User, format!("message-{index}"))
                .unwrap();
        }
        registry
            .record_message(
                &session.id,
                JevRole::Assistant,
                "x".repeat(MAX_CONTEXT_MESSAGE_CHARS + 50),
            )
            .unwrap();

        let context = registry.routing_context(&session.id).unwrap();
        assert_eq!(context.recent_messages.len(), MAX_RECENT_MESSAGES);
        assert_eq!(
            context.recent_messages.first().unwrap().content,
            "message-4"
        );
        let last = context.recent_messages.last().unwrap();
        assert_eq!(last.role, JevRole::Assistant);
        assert_eq!(last.content.chars().count(), MAX_CONTEXT_MESSAGE_CHARS + 1);
        assert!(last.content.ends_with('…'));
    }

    #[test]
    fn corrupt_durable_file_fails_without_overwriting_the_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.json");
        let corrupt = b"{ definitely not valid json";
        std::fs::write(&path, corrupt).unwrap();

        assert!(matches!(
            SessionRegistry::with_state_dir(temp.path()),
            Err(SessionError::CorruptState(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
    }

    #[test]
    fn explicit_cancel_removes_the_durable_record() {
        let temp = tempfile::tempdir().unwrap();
        let registry = SessionRegistry::with_state_dir(temp.path()).unwrap();
        let session = registry
            .new_session("claude", None, None, None, &registry_with_all())
            .unwrap();
        assert!(registry.cancel_result(&session.id).unwrap());
        assert!(registry.get(&session.id).is_none());

        let reloaded = SessionRegistry::with_state_dir(temp.path()).unwrap();
        assert!(reloaded.get(&session.id).is_none());
        assert!(reloaded.list().is_empty());
    }
}
