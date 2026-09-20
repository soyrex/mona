//! ACP stdio server loop. Reads JSON-RPC frames from stdin, dispatches
//! to the appropriate route handler, and writes responses to stdout.
//!
//! This is a hand-rolled JSON-RPC layer. The upstream `crates/jcode/src/cli/acp.rs`
//! uses the same pattern (it's how upstream's `jcode acp` already speaks to
//! Monitter's ACP clients). We follow that pattern so the wire format is
//! identical.
//!
//! The server uses newline-delimited JSON (`\n`-terminated frames). This is
//! what the Monitter `acp_runtime.rs` already speaks.
//!
//! Phase 2 scope: initialize, session/new, session/resume, session/cancel,
//! session/list, session/set_model, session/set_reasoning_effort.
//! `session/prompt` returns a stub response (Phase 2.5 wires the real
//! `Agent::run_turn_with_jev` path).

use crate::auth::{Auth, AuthRegistry};
use crate::initialize::initialize_result;
use crate::policy::JevRoutePolicy;
use crate::provider_whitelist::parse_provider;
use crate::session::{SessionInfo, SessionRegistry};
use crate::trace::TraceTrigger;
use crate::turn::{RouterConfig, decision_to_router_trace_value, run_turn_with_jev};
use anyhow::{Context, Result};
use mona_jev::JevClassifier;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

/// Build the server name + version reported in `initialize`.
pub const SERVER_NAME: &str = "mona-acp";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared, mutable server state. Held behind a Mutex because the session
/// registry needs interior mutability.
#[derive(Clone)]
pub struct ServerState {
    pub sessions: SessionRegistry,
    pub policy: Arc<Mutex<JevRoutePolicy>>,
    pub last_classification: Arc<Mutex<Option<mona_jev::JevRoutePlan>>>,
    pub classifier: Arc<dyn JevClassifier>,
    pub router_config: Arc<RouterConfig>,
    pub home_dir: PathBuf,
    pub auth: AuthRegistry,
    /// Per-session turn counter + swaps counter, kept here because the
    /// session registry is per-session and we'd otherwise lose the counts.
    pub session_counters: Arc<Mutex<SessionCounters>>,
}

#[derive(Default, Clone)]
struct SessionCounters {
    /// session_id -> (turn_count, swaps_count)
    inner: std::collections::HashMap<String, (u32, u32)>,
}

impl SessionCounters {
    fn get_or_insert(&mut self, id: &str) -> &mut (u32, u32) {
        self.inner.entry(id.to_string()).or_insert((0, 0))
    }
    fn record_swap(&mut self, id: &str) {
        let entry = self.inner.entry(id.to_string()).or_insert((0, 0));
        entry.1 += 1;
    }
    #[allow(dead_code)] // kept for future use when session/cancel is wired
    fn remove(&mut self, id: &str) {
        self.inner.remove(id);
    }
}

impl ServerState {
    pub fn new(home_dir: PathBuf, classifier: Arc<dyn JevClassifier>) -> Self {
        let auth = AuthRegistry::load(&home_dir);
        Self {
            sessions: SessionRegistry::default(),
            policy: Arc::new(Mutex::new(JevRoutePolicy::SafeAuto)),
            last_classification: Arc::new(Mutex::new(None)),
            classifier,
            router_config: Arc::new(RouterConfig::default()),
            home_dir,
            auth,
            session_counters: Arc::new(Mutex::new(SessionCounters::default())),
        }
    }

    pub fn with_classifier(mut self, classifier: Arc<dyn JevClassifier>) -> Self {
        self.classifier = classifier;
        self
    }
}

/// Run the ACP server loop on `stdin`/`stdout` until EOF on stdin.
pub async fn run_acp_server(state: ServerState) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut writer = tokio::io::BufWriter::new(stdout);
    let mut buf = String::new();

    info!("mona-acp listening on stdio");
    loop {
        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .await
            .context("read from stdin")?;
        if n == 0 {
            info!("stdin closed; shutting down mona-acp");
            return Ok(());
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }

        let response = handle_frame(line, &state).await;
        let response_str = match serde_json::to_string(&response) {
            Ok(s) => s,
            Err(e) => {
                error!("failed to serialize response: {e}");
                continue;
            }
        };
        writer.write_all(response_str.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
}

/// Dispatch one JSON-RPC frame and return the response.
pub async fn handle_frame(line: &str, state: &ServerState) -> Value {
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return jsonrpc_error(Value::Null, -32700, &format!("invalid JSON: {e}"));
        }
    };

    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = match request.get("method").and_then(Value::as_str) {
        Some(m) => m.to_string(),
        None => return jsonrpc_error(id, -32600, "missing method"),
    };
    let params = request.get("params").cloned().unwrap_or(json!({}));

    debug!(method = %method, "dispatching ACP frame");
    match method.as_str() {
        "initialize" => handle_initialize(id, &params, state),
        "session/new" => handle_session_new(id, &params, state).await,
        "session/list" => handle_session_list(id, state),
        "session/resume" => handle_session_resume(id, &params, state),
        "session/cancel" => handle_session_cancel(id, &params, state),
        "session/auth" => handle_session_auth(id, &params, state),
        "session/prompt" => handle_session_prompt_stub(id, &params, state).await,
        "session/set_model" => handle_session_set_model(id, &params, state),
        "session/set_reasoning_effort" => handle_session_set_reasoning_effort(id, &params, state),
        other => jsonrpc_error(
            id,
            -32601,
            &format!("method `{other}` is not implemented in mona-acp Phase 2"),
        ),
    }
}

fn handle_initialize(id: Value, _params: &Value, state: &ServerState) -> Value {
    jsonrpc_result(id, initialize_result(SERVER_NAME, SERVER_VERSION, &state.auth))
}

async fn handle_session_new(id: Value, params: &Value, state: &ServerState) -> Value {
    let provider = params
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let model = params.get("model").and_then(Value::as_str);
    let effort = params.get("effort").and_then(Value::as_str);
    let working_dir = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    match state.sessions.new_session(provider, model, effort, working_dir) {
        Ok(session) => {
            info!(session_id = %session.id, provider = %session.provider.as_str(),
                  model = %session.model, "created session");
            let info: SessionInfo = (&session).into();
            jsonrpc_result(
                id,
                json!({
                    "sessionId": info.session_id,
                    "provider": info.provider,
                    "model": info.model,
                    "effort": info.effort,
                    "monitterPhase": "2",
                    "jev_routing": true,
                }),
            )
        }
        Err(e) => jsonrpc_error(id, -32602, &format!("{e}")),
    }
}

fn handle_session_list(id: Value, state: &ServerState) -> Value {
    let sessions: Vec<Value> = state
        .sessions
        .list()
        .iter()
        .map(|s| {
            let info: SessionInfo = s.into();
            json!({
                "sessionId": info.session_id,
                "provider": info.provider,
                "model": info.model,
                "effort": info.effort,
                "createdAt": s.created_at,
            })
        })
        .collect();
    jsonrpc_result(id, json!({ "sessions": sessions }))
}

fn handle_session_resume(id: Value, params: &Value, state: &ServerState) -> Value {
    let session_id = match params.get("sessionId").and_then(Value::as_str) {
        Some(s) => s,
        None => return jsonrpc_error(id, -32602, "missing sessionId"),
    };
    match state.sessions.get(session_id) {
        Some(s) => {
            let info: SessionInfo = (&s).into();
            jsonrpc_result(
                id,
                json!({
                    "sessionId": info.session_id,
                    "provider": info.provider,
                    "model": info.model,
                    "effort": info.effort,
                    "resumed": true,
                }),
            )
        }
        None => jsonrpc_error(id, -32004, &format!("session `{session_id}` not found")),
    }
}

fn handle_session_cancel(id: Value, params: &Value, state: &ServerState) -> Value {
    let session_id = match params.get("sessionId").and_then(Value::as_str) {
        Some(s) => s,
        None => return jsonrpc_error(id, -32602, "missing sessionId"),
    };
    if state.sessions.cancel(session_id) {
        info!(session_id, "session cancelled");
        jsonrpc_result(id, json!({ "cancelled": true }))
    } else {
        warn!(session_id, "session/cancel for unknown session");
        jsonrpc_result(id, json!({ "cancelled": false }))
    }
}

/// `session/auth` — report the auth state for a session's provider.
///
/// Returns `{ "configured": bool, "summary": <masked credential> | null,
/// "phase": "3" }`. Useful for the Monitter UI to show a "Connect
/// provider" affordance when the session's provider has no auth.
fn handle_session_auth(id: Value, params: &Value, state: &ServerState) -> Value {
    let session_id = match params.get("sessionId").and_then(Value::as_str) {
        Some(s) => s,
        None => return jsonrpc_error(id, -32602, "missing sessionId"),
    };
    let session = match state.sessions.get(session_id) {
        Some(s) => s,
        None => return jsonrpc_error(id, -32004, &format!("session `{session_id}` not found")),
    };

    match state.auth.get(session.provider) {
        Some(auth) => jsonrpc_result(
            id,
            json!({
                "configured": true,
                "summary": auth.masked_summary(),
                "provider": session.provider.as_str(),
                "phase": "3"
            }),
        ),
        None => jsonrpc_result(
            id,
            json!({
                "configured": false,
                "summary": null,
                "provider": session.provider.as_str(),
                "phase": "3",
                "hint": format!(
                    "place credentials at ~/.mona/{}.json or set the matching env var",
                    session.provider.as_str()
                )
            }),
        ),
    }
}

/// Per-turn routing hook + agent-loop stub.
///
/// Phase 2.5: runs the Jev classifier, applies safety gates, persists a
/// per-turn trace, and emits a `router_trace` push event to the client.
/// The actual `Agent::run_turn()` integration is still stubbed (returns an
/// ack); Phase 3 wires the stub to the real agent loop.
async fn handle_session_prompt_stub(id: Value, params: &Value, state: &ServerState) -> Value {
    let session_id = match params.get("sessionId").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return jsonrpc_error(id, -32602, "missing sessionId"),
    };
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");

    let mut session = match state.sessions.get(&session_id) {
        Some(s) => s,
        None => return jsonrpc_error(id, -32004, &format!("session `{session_id}` not found")),
    };

    // Read the current turn counter BEFORE incrementing, so the very
    // first prompt in a session sees turn_count = 0 (no cooldown). Then
    // increment so the next prompt sees turn_count = 1.
    let (turn_count_for_routing, swaps_in_session) = {
        let counters = state.session_counters.lock().await;
        let entry = counters.inner.get(&session_id).copied().unwrap_or((0, 0));
        (entry.0, entry.1)
    };
    {
        let mut counters = state.session_counters.lock().await;
        counters.get_or_insert(&session_id).0 += 1;
    }

    // 1. Run the per-turn router
    let decision = match run_turn_with_jev(
        state.classifier.clone(),
        &mut session,
        text,
        turn_count_for_routing,
        swaps_in_session,
        None, // last_turn_outcome — populated when Agent::run_turn drives this
        &state.router_config,
        &state.home_dir,
    )
    .await
    {
        Ok(d) => d,
        Err(e) => {
            error!(error = %e, session_id, "router hook errored");
            return jsonrpc_error(id, -32603, &format!("router hook error: {e}"));
        }
    };

    // 2. Update session counters if a swap happened
    if decision.trace.applied {
        let mut counters = state.session_counters.lock().await;
        counters.record_swap(&session_id);
    }

    // 3. Persist the updated session state (model + effort changes)
    state.sessions.update(&session);

    // 4. Cache the latest plan for inspection
    *state.last_classification.lock().await = decision.applied_plan.clone();

    // 5. Build the JSON-RPC response. Sensitive prompts short-circuit to
    //    permission_required; everything else returns the routing decision.
    //
    //    `decision.sensitive` is propagated independently of the safety
    //    gate's verdict — even when the gate refuses for cooldown or low
    //    confidence, a sensitive plan still routes to human review.
    if decision.sensitive {
        return jsonrpc_error(
            id,
            -32001, // permission_required
            "sensitive prompt detected; routing to human review required",
        );
    }
    let response = if decision.applied_plan.is_some() {
        json!({
            "sessionId": session_id,
            "stopReason": "phase2.5_routing_done",
            "model": decision.new_model,
            "effort": decision.new_effort,
            "applied": decision.trace.applied,
            "tier": format!("{:?}", decision.trace.proposed_tier.unwrap_or(mona_jev::ModelTier::Balanced)).to_lowercase(),
            "reason": decision.reason,
            "usage": { "inputTokens": text.len(), "outputTokens": 0 }
        })
    } else {
        json!({
            "sessionId": session_id,
            "stopReason": "phase2.5_routing_refused",
            "model": decision.new_model,
            "effort": decision.new_effort,
            "applied": false,
            "reason": decision.reason,
            "usage": { "inputTokens": text.len(), "outputTokens": 0 }
        })
    };

    // Phase 3 will: drive Agent::run_turn here, stream events, return the
    // final text. For now we return the routing decision as the response.
    debug!(session_id, "session/prompt routing done; agent loop stub");
    jsonrpc_result(id, response)
}

fn handle_session_set_model(id: Value, params: &Value, state: &ServerState) -> Value {
    let session_id = match params.get("sessionId").and_then(Value::as_str) {
        Some(s) => s,
        None => return jsonrpc_error(id, -32602, "missing sessionId"),
    };
    let new_model = match params.get("model").and_then(Value::as_str) {
        Some(m) => m,
        None => return jsonrpc_error(id, -32602, "missing model"),
    };

    let session = match state.sessions.get(session_id) {
        Some(s) => s,
        None => return jsonrpc_error(id, -32004, &format!("session `{session_id}` not found")),
    };

    // Phase 2: validate the requested model against the session's provider
    // whitelist. Phase 2.5 will route through `set_model_with_auth_refresh`.
    if parse_provider(new_model).is_err() && !is_model_under_provider(&new_model, session.provider.as_str()) {
        return jsonrpc_error(
            id,
            -32602,
            &format!(
                "model `{new_model}` is not compatible with provider `{}`",
                session.provider.as_str()
            ),
        );
    }

    info!(session_id, old_model = %session.model, new_model, "model set");
    // Note: actual model mutation happens in the registry in Phase 2.5 when
    // we have a write-locked Session. For Phase 2, we report success and
    // log the change.
    jsonrpc_result(
        id,
        json!({
            "model": new_model,
            "phase": "2",
            "note": "model change recorded; live provider swap lands in Phase 2.5"
        }),
    )
}

fn handle_session_set_reasoning_effort(id: Value, params: &Value, state: &ServerState) -> Value {
    let session_id = match params.get("sessionId").and_then(Value::as_str) {
        Some(s) => s,
        None => return jsonrpc_error(id, -32602, "missing sessionId"),
    };
    let effort = match params.get("effort").and_then(Value::as_str) {
        Some(e) => e,
        None => return jsonrpc_error(id, -32602, "missing effort"),
    };

    if state.sessions.get(session_id).is_none() {
        return jsonrpc_error(id, -32004, &format!("session `{session_id}` not found"));
    }

    info!(session_id, effort, "reasoning effort set");
    jsonrpc_result(
        id,
        json!({
            "effort": effort,
            "phase": "2",
            "note": "effort change recorded; live provider swap lands in Phase 2.5"
        }),
    )
}

fn is_model_under_provider(model: &str, provider: &str) -> bool {
    // Lightweight validation: each provider has a recognizable prefix in the
    // model name. Phase 2.5 uses the full `Provider::available_models()`.
    let m = model.to_ascii_lowercase();
    match provider {
        "codex" => m.starts_with("gpt-") || m.starts_with("o") || m.starts_with("codex"),
        "claude" => m.starts_with("claude") || m.contains("sonnet") || m.contains("opus") || m.contains("haiku"),
        "minimax" => m.starts_with("minimax") || m.starts_with("abab"),
        _ => false,
    }
}

fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn jsonrpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}
