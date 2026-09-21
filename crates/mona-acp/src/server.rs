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
//! The server supports session lifecycle, streamed provider turns, and a
//! bounded tool continuation loop. Mutating tools use full-duplex ACP
//! `session/request_permission` requests; stdout remains JSON-RPC-only.

use crate::auth::AuthRegistry;
use crate::initialize::initialize_result;
use crate::policy::JevRoutePolicy;
use crate::session::{SessionInfo, SessionRegistry};
use crate::turn::{
    RouterConfig, decision_to_router_trace_value, finalize_routing_decision, run_turn_with_jev,
};
use anyhow::{Context, Result};
use futures::StreamExt;
use mona_jev::JevClassifier;
use mona_message_types::{ContentBlock, Message, Role, StreamEvent, ToolCall};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info, warn};

/// Shared stdout writer for the ACP server. Handlers that stream updates
/// mid-turn (notably `session/prompt`) lock this for the duration of each
/// push so notification ordering is deterministic.
///
/// Wrapped in a boxed `AsyncWrite` trait object so tests can substitute an
/// in-memory buffer for `Stdout`.
pub type SharedWriter = Arc<Mutex<BufWriter<Box<dyn AsyncWrite + Send + Unpin>>>>;
type PendingClientResponses = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

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
    session_counters: Arc<Mutex<SessionCounters>>,
    /// In-server tool registry. `read_file`, `write_file`, `bash`,
    /// `ls` by default. Hosts can register additional tools before
    /// launching the server.
    pub tools: Arc<mona_acp_tools::ToolRegistry>,
    /// Responses to server-initiated JSON-RPC requests, principally
    /// `session/request_permission`. The stdin reader remains live while a
    /// prompt task waits on the matching oneshot sender.
    pending_client_responses: PendingClientResponses,
    client_closed: Arc<AtomicBool>,
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
            tools: Arc::new(mona_acp_tools::default_registry()),
            pending_client_responses: Arc::new(Mutex::new(HashMap::new())),
            client_closed: Arc::new(AtomicBool::new(false)),
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
    let writer: SharedWriter = Arc::new(Mutex::new(BufWriter::new(Box::new(stdout))));
    let mut buf = String::new();
    let (request_tx, mut request_rx) = mpsc::unbounded_channel::<String>();
    let worker_state = state.clone();
    let worker_writer = writer.clone();
    let request_worker = tokio::spawn(async move {
        while let Some(line) = request_rx.recv().await {
            let response = handle_frame(&line, &worker_state, worker_writer.clone()).await;
            if let Err(error) = write_json_frame(&worker_writer, &response).await {
                error!(%error, "failed to write ACP response");
                break;
            }
        }
    });

    info!("mona-acp listening on stdio");
    loop {
        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .await
            .context("read from stdin")?;
        if n == 0 {
            info!("stdin closed; shutting down mona-acp");
            drop(request_tx);
            // Dropping outstanding response senders rejects permission waits
            // promptly instead of leaving the ordered worker blocked.
            state.client_closed.store(true, Ordering::Release);
            state.pending_client_responses.lock().await.clear();
            if let Err(error) = request_worker.await {
                warn!(%error, "ACP request worker stopped unexpectedly");
            }
            return Ok(());
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }

        if let Ok(frame) = serde_json::from_str::<Value>(line)
            && frame.get("method").is_none()
            && frame.get("id").is_some()
        {
            if !deliver_client_response(&state.pending_client_responses, frame).await {
                warn!("ignoring response for unknown or expired server request");
            }
            continue;
        }

        if request_tx.send(line.to_string()).is_err() {
            anyhow::bail!("ACP request worker is unavailable");
        }
    }
}

async fn write_json_frame(writer: &SharedWriter, frame: &Value) -> std::io::Result<()> {
    let encoded = serde_json::to_vec(frame).map_err(std::io::Error::other)?;
    let mut writer = writer.lock().await;
    writer.write_all(&encoded).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

async fn deliver_client_response(pending: &PendingClientResponses, frame: Value) -> bool {
    let Some(id) = frame.get("id") else {
        return false;
    };
    let key = id.to_string();
    let sender = pending.lock().await.remove(&key);
    match sender {
        Some(sender) => {
            let _ = sender.send(frame);
            true
        }
        None => false,
    }
}

/// Dispatch one JSON-RPC frame and return the response.
pub async fn handle_frame(line: &str, state: &ServerState, writer: SharedWriter) -> Value {
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
        "session/prompt" => handle_session_prompt(id, &params, state, writer).await,
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
    jsonrpc_result(
        id,
        initialize_result(SERVER_NAME, SERVER_VERSION, &state.auth),
    )
}

async fn handle_session_new(id: Value, params: &Value, state: &ServerState) -> Value {
    let provider_str = params
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let model = params.get("model").and_then(Value::as_str);
    let effort = params.get("effort").and_then(Value::as_str);
    let working_dir = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    let provider =
        match crate::provider_whitelist::parse_provider_with_default(provider_str, &state.auth) {
            Ok(p) => p,
            Err(e) => return jsonrpc_error(id, -32602, &format!("{e}")),
        };
    // Reuse the provider_str for the existing SessionRegistry call. The
    // registry also calls parse_provider; if it errors with "auto", we
    // catch that above and never reach here.
    match state
        .sessions
        .new_session(provider.as_str(), model, effort, working_dir, &state.auth)
    {
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
                    "providerName": info.provider_name,
                    "monitterPhase": "3.5",
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

/// Per-turn routing hook + agent-loop driver.
///
/// Milestones B and C.1 drive a real model turn through the session's
/// `ProviderHandle`, streams `StreamEvent`s as `session/update` push
/// notifications, execute advertised tools, and return the final assistant
/// text plus token usage.
///
/// Scope:
/// - One user message per `session/prompt` call; no conversation history is
///   kept inside mona-acp yet.
/// - Tool calls are bounded to eight provider rounds. `bash` and `write_file`
///   require a one-time ACP permission response; scoped read tools do not.
/// - `session/cancel` mid-stream is not yet wired; the turn runs to
///   completion or transport error.
/// - OAuth refresh during a turn surfaces as `unauthenticated`.
///
/// Per-turn Jev routing applies its provider-scoped model and effort to an
/// isolated provider candidate. Only a fully successful candidate replaces
/// the session runtime; failures are traced and the previous runtime remains
/// active.
async fn handle_session_prompt(
    id: Value,
    params: &Value,
    state: &ServerState,
    writer: SharedWriter,
) -> Value {
    let session_id = match params.get("sessionId").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return jsonrpc_error(id, -32602, "missing sessionId"),
    };
    // Standard ACP `session/prompt` uses `prompt` as an array of content
    // blocks. mona-acp's own smoke harness used a flat `text` string.
    // Accept both shapes so real ACP clients (Monitter, OpenCode ACP
    // bridge, ...) work without a custom envelope.
    let text = extract_prompt_text(params);

    let mut session = match state.sessions.get(&session_id) {
        Some(s) => s,
        None => return jsonrpc_error(id, -32004, &format!("session `{session_id}` not found")),
    };

    // 1. Run the per-turn router. Sensitive prompts short-circuit to
    //    permission_required before we touch the provider.
    let (turn_count_for_routing, swaps_in_session) = {
        let counters = state.session_counters.lock().await;
        let entry = counters.inner.get(&session_id).copied().unwrap_or((0, 0));
        (entry.0, entry.1)
    };
    {
        let mut counters = state.session_counters.lock().await;
        counters.get_or_insert(&session_id).0 += 1;
    }
    let mut decision = match run_turn_with_jev(
        state.classifier.clone(),
        &session,
        &text,
        turn_count_for_routing,
        swaps_in_session,
        None, // last_turn_outcome — populated once we have a real Agent loop
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
    *state.last_classification.lock().await = decision.applied_plan.clone();

    if decision.sensitive {
        if let Err(error) =
            finalize_routing_decision(&mut decision, &session, false, None, &state.home_dir).await
        {
            return jsonrpc_error(id, -32603, &format!("persist router trace: {error}"));
        }
        return jsonrpc_error(
            id,
            -32001, // permission_required
            "sensitive prompt detected; routing to human review required",
        );
    }

    // 2. Apply a safety-approved route to an isolated provider candidate.
    //    A failed model/effort change leaves this session and its live handle
    //    untouched, then the prompt continues on the old runtime.
    let route_requested = decision.applied_plan.is_some();
    let pre_route_session = session.clone();
    let mut route_error = None;
    if route_requested {
        let authenticated = session
            .handle
            .as_ref()
            .is_some_and(|handle| handle.auth.is_some());
        if !authenticated {
            route_error = Some("provider is not authenticated".to_string());
        } else if let (Some(model), Some(effort)) = (
            decision.requested_model.as_deref(),
            decision.requested_effort.as_deref(),
        ) {
            let old_model = session.model.clone();
            let old_effort = session.effort.clone();
            match session.reconfigure_provider(model, effort) {
                Ok(()) => {
                    if !state.sessions.update(&session) {
                        route_error = Some("session disappeared while applying route".to_string());
                    } else if session.model != old_model || session.effort != old_effort {
                        state.session_counters.lock().await.record_swap(&session_id);
                    }
                }
                Err(error) => {
                    warn!(%error, session_id, model, effort, "live Jev route rolled back");
                    route_error = Some(error.to_string());
                }
            }
        } else {
            route_error = Some("router omitted a concrete model or effort".to_string());
        }
    }
    if route_error.is_some() {
        session = pre_route_session;
    }
    let route_applied = route_requested && route_error.is_none();
    if let Err(error) = finalize_routing_decision(
        &mut decision,
        &session,
        route_applied,
        route_error,
        &state.home_dir,
    )
    .await
    {
        return jsonrpc_error(id, -32603, &format!("persist router trace: {error}"));
    }
    let routing = decision_to_router_trace_value(&decision);
    push_notification(
        &writer,
        &session_id,
        json!({
            "sessionUpdate": "router_trace",
            "trace": routing.clone(),
        }),
    )
    .await;

    // 3. Drive the real provider. Sessions created without auth attach a
    //    placeholder handle whose `provider_name` is null AND whose
    //    `auth` is None; trying to `complete` on it would fail inside the
    //    provider. We surface that as `unauthenticated` (-32002) here so
    //    the client can call `session/auth` and retry instead of getting
    //    a generic internal_error.
    let handle = match session.handle.as_ref() {
        Some(h) if h.auth.is_some() => h.clone(),
        _ => {
            return jsonrpc_error(
                id,
                -32002, // unauthenticated
                &format!(
                    "session `{}` has no configured auth for provider `{}`; \
                     call session/auth or recreate the session with credentials",
                    session_id,
                    session.provider.as_str()
                ),
            );
        }
    };
    debug!(
        session_id,
        provider = %handle.provider.name(),
        model = %handle.provider.model(),
        "session/prompt driving provider"
    );

    let cwd = session
        .working_dir
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    if !cwd.is_dir() {
        return jsonrpc_error(
            id,
            -32602,
            &format!(
                "session working directory does not exist: {}",
                cwd.display()
            ),
        );
    }
    let permission = AcpPermission::new(
        session_id.clone(),
        writer.clone(),
        state.pending_client_responses.clone(),
        state.client_closed.clone(),
    );

    // 4. Drive the real provider through the shared streaming loop.
    //    `drive_provider_stream` is the testable core; this handler just
    //    maps its outcome back to a JSON-RPC response.
    let outcome = match drive_provider_stream(
        &handle,
        &state.tools,
        &cwd,
        vec![Message::user(&text)],
        &writer,
        &session_id,
        &permission,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return jsonrpc_error(id, -32603, &format!("session/prompt failed: {e}")),
    };

    let (input_tokens, output_tokens) = outcome.usage.unwrap_or((0, outcome.text.len() as u64));
    jsonrpc_result(
        id,
        json!({
            "sessionId": session_id,
            "stopReason": outcome.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
            "model": handle.provider.model(),
            "effort": session.effort,
            "output": outcome.text,
            "routing": routing,
            "usage": {
                "inputTokens": input_tokens,
                "outputTokens": output_tokens,
            },
        }),
    )
}

/// Aggregated outcome of a single provider turn. Captures the assistant
/// text, the last `MessageEnd::stop_reason`, the last full token usage,
/// and any transport/provider error that interrupted the stream.
#[derive(Debug, Default, Clone)]
pub(crate) struct PromptOutcome {
    pub text: String,
    pub stop_reason: Option<String>,
    pub usage: Option<(u64, u64)>,
    pub error: Option<String>,
}

/// One permission decision the host (Monitter) must make before mona-acp
/// invokes a tool that requires it. Mirrors ACP's request_permission
/// shape so the existing Monitter UI keeps working.
#[derive(Debug, Clone)]
struct PermissionRequest {
    pub request_id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
}

/// What the host decides.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PermissionDecision {
    AllowOnce,
    RejectOnce,
}

/// Host-side permission boundary. `drive_provider_stream` calls
/// `request(&req)` whenever a tool whose `permission()` is `Required`
/// is about to run. Production uses `AcpPermission`; tests provide bounded
/// fakes for deterministic allow/deny coverage.
trait Permission: Send + Sync {
    fn request<'a>(
        &'a self,
        req: &'a PermissionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PermissionDecision> + Send + 'a>>;
}

#[cfg(test)]
struct AlwaysAllow;

#[cfg(test)]
impl Permission for AlwaysAllow {
    fn request<'a>(
        &'a self,
        _req: &'a PermissionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PermissionDecision> + Send + 'a>> {
        Box::pin(async { PermissionDecision::AllowOnce })
    }
}

struct AcpPermission {
    session_id: String,
    writer: SharedWriter,
    pending: PendingClientResponses,
    client_closed: Arc<AtomicBool>,
}

impl AcpPermission {
    fn new(
        session_id: String,
        writer: SharedWriter,
        pending: PendingClientResponses,
        client_closed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            session_id,
            writer,
            pending,
            client_closed,
        }
    }
}

impl Permission for AcpPermission {
    fn request<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PermissionDecision> + Send + 'a>> {
        Box::pin(async move {
            if self.client_closed.load(Ordering::Acquire) {
                return PermissionDecision::RejectOnce;
            }
            let request_id = Value::String(format!("mona-permission-{}", uuid::Uuid::new_v4()));
            let allow_option = format!("allow-once-{}", uuid::Uuid::new_v4());
            let reject_option = format!("reject-once-{}", uuid::Uuid::new_v4());
            let (sender, receiver) = oneshot::channel();
            self.pending
                .lock()
                .await
                .insert(request_id.to_string(), sender);

            let frame = json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "session/request_permission",
                "params": {
                    "sessionId": self.session_id,
                    "toolCall": {
                        "toolCallId": request.request_id,
                        "toolName": request.tool_name,
                        "title": request.tool_name,
                        "kind": "execute",
                        "rawInput": request.input,
                    },
                    "options": [
                        {
                            "optionId": allow_option,
                            "name": "Allow once",
                            "kind": "allow_once",
                        },
                        {
                            "optionId": reject_option,
                            "name": "Reject",
                            "kind": "reject_once",
                        }
                    ]
                }
            });
            if write_json_frame(&self.writer, &frame).await.is_err() {
                self.pending.lock().await.remove(&request_id.to_string());
                return PermissionDecision::RejectOnce;
            }

            let response = match timeout(Duration::from_secs(300), receiver).await {
                Ok(Ok(response)) => response,
                _ => {
                    self.pending.lock().await.remove(&request_id.to_string());
                    return PermissionDecision::RejectOnce;
                }
            };
            let outcome = &response["result"]["outcome"];
            if outcome["outcome"] == "selected"
                && outcome["optionId"].as_str() == Some(allow_option.as_str())
            {
                PermissionDecision::AllowOnce
            } else {
                PermissionDecision::RejectOnce
            }
        })
    }
}

/// Open `Provider::complete` for the given messages and drain the stream,
/// emitting one `session/update` push notification per relevant event.
///
/// This is the testable core of `session/prompt`; unit tests in this module
/// drive it directly with a mock provider and an in-memory writer, without
/// spinning up a child process or polluting stdout.
///
/// Milestone C.1: when the model emits a standard streamed tool call or a
/// `NativeToolCall`, mona-acp asks the host for permission (if the tool
/// requires it), runs the tool via `mona-acp-tools`, folds the result back
/// into the messages, and re-enters `provider.complete()` for another round.
/// The function returns once the model emits `MessageEnd` without another
/// tool call.
async fn drive_provider_stream(
    handle: &crate::provider::ProviderHandle,
    tools: &mona_acp_tools::ToolRegistry,
    cwd: &PathBuf,
    mut messages: Vec<Message>,
    writer: &SharedWriter,
    session_id: &str,
    permission: &(dyn Permission + Send + Sync),
) -> anyhow::Result<PromptOutcome> {
    debug!(
        session_id,
        provider = %handle.provider.name(),
        model = %handle.provider.model(),
        "drive_provider_stream opening provider.complete"
    );

    let tool_defs = tools.definitions();
    let mut outcome = PromptOutcome::default();

    // Cap the blast radius if the model loops on a tool or repeatedly asks
    // for an action the user denied.
    const MAX_TOOL_ROUNDS: usize = 8;
    for round in 0..MAX_TOOL_ROUNDS {
        outcome.stop_reason = None;
        let round_output_start = outcome.text.len();
        let round_usage_start = outcome.usage;
        let mut stream = handle
            .provider
            .complete(&messages, &tool_defs, "", None)
            .await
            .map_err(|e| {
                anyhow::anyhow!("provider `{}` open failed: {e}", handle.provider.name())
            })?;

        let mut round_text = String::new();
        let mut current_tool: Option<ToolCall> = None;
        let mut current_tool_input = String::new();
        let mut round_tool_calls: Vec<ToolCall> = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta(delta)) => {
                    outcome.text.push_str(&delta);
                    round_text.push_str(&delta);
                    push_notification(
                        writer,
                        session_id,
                        json!({
                            "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": delta },
                        }),
                    )
                    .await;
                }
                Ok(StreamEvent::TextDone) => {
                    push_notification(
                        writer,
                        session_id,
                        json!({ "sessionUpdate": "agent_message_done" }),
                    )
                    .await;
                }
                Ok(StreamEvent::ThinkingStart) => {
                    push_notification(
                        writer,
                        session_id,
                        json!({ "sessionUpdate": "agent_thought_chunk" }),
                    )
                    .await;
                }
                Ok(StreamEvent::ThinkingDelta(delta)) => {
                    push_notification(
                        writer,
                        session_id,
                        json!({
                            "sessionUpdate": "agent_thought_chunk",
                            "content": { "type": "text", "text": delta },
                        }),
                    )
                    .await;
                }
                Ok(StreamEvent::ThinkingEnd) | Ok(StreamEvent::ThinkingDone { .. }) => {
                    push_notification(
                        writer,
                        session_id,
                        json!({ "sessionUpdate": "agent_thought_chunk", "phase": "end" }),
                    )
                    .await;
                }
                Ok(StreamEvent::ToolUseStart { id, name }) => {
                    current_tool = Some(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: Value::Null,
                        intent: None,
                        thought_signature: None,
                    });
                    current_tool_input.clear();
                    push_notification(
                        writer,
                        session_id,
                        json!({
                            "sessionUpdate": "tool_call",
                            "toolCallId": id,
                            "title": name,
                            "status": "pending",
                        }),
                    )
                    .await;
                }
                Ok(StreamEvent::ToolInputDelta(delta)) => {
                    let Some(tool) = current_tool.as_ref() else {
                        continue;
                    };
                    current_tool_input.push_str(&delta);
                    push_notification(
                        writer,
                        session_id,
                        json!({
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": tool.id,
                            "rawInputDelta": delta,
                        }),
                    )
                    .await;
                }
                Ok(StreamEvent::ToolUseEnd) => {
                    if let Some(mut tool) = current_tool.take() {
                        tool.input = ToolCall::parse_streamed_input_to_object(&current_tool_input);
                        tool.intent = ToolCall::intent_from_input(&tool.input);
                        current_tool_input.clear();
                        push_notification(
                            writer,
                            session_id,
                            json!({
                                "sessionUpdate": "tool_call_update",
                                "toolCallId": tool.id,
                                "status": "in_progress",
                                "rawInput": tool.input,
                            }),
                        )
                        .await;
                        round_tool_calls.push(tool);
                    }
                }
                Ok(StreamEvent::ToolUseSignature(signature)) => {
                    if let Some(tool) = round_tool_calls.last_mut()
                        && !signature.is_empty()
                    {
                        tool.thought_signature = Some(signature);
                    }
                }
                Ok(StreamEvent::NativeToolCall {
                    request_id,
                    tool_name,
                    input,
                }) => {
                    if current_tool
                        .as_ref()
                        .is_some_and(|tool| tool.id == request_id)
                    {
                        let mut tool = current_tool.take().expect("matching tool exists");
                        tool.input = input;
                        tool.intent = ToolCall::intent_from_input(&tool.input);
                        current_tool_input.clear();
                        round_tool_calls.push(tool);
                    } else if !round_tool_calls.iter().any(|tool| tool.id == request_id) {
                        push_notification(
                            writer,
                            session_id,
                            json!({
                                "sessionUpdate": "tool_call",
                                "toolCallId": request_id,
                                "title": tool_name,
                                "status": "in_progress",
                                "rawInput": input,
                            }),
                        )
                        .await;
                        round_tool_calls.push(ToolCall {
                            id: request_id,
                            name: tool_name,
                            input,
                            intent: None,
                            thought_signature: None,
                        });
                    }
                }
                Ok(StreamEvent::TokenUsage {
                    input_tokens,
                    output_tokens,
                    ..
                }) => {
                    if let (Some(i), Some(o)) = (input_tokens, output_tokens) {
                        let (prior_input, prior_output) = outcome.usage.unwrap_or((0, 0));
                        outcome.usage = Some((prior_input + i, prior_output + o));
                    }
                    push_notification(
                        writer,
                        session_id,
                        json!({
                            "sessionUpdate": "usage_update",
                            "inputTokens": input_tokens,
                            "outputTokens": output_tokens,
                        }),
                    )
                    .await;
                }
                Ok(StreamEvent::MessageEnd { stop_reason }) => {
                    outcome.stop_reason = stop_reason;
                }
                Ok(StreamEvent::Error {
                    message,
                    retry_after_secs,
                }) => {
                    outcome.error = Some(message.clone());
                    push_notification(
                        writer,
                        session_id,
                        json!({
                            "sessionUpdate": "error",
                            "message": message,
                            "retryAfterSecs": retry_after_secs,
                        }),
                    )
                    .await;
                    break;
                }
                Ok(StreamEvent::RetryRollback { .. }) => {
                    outcome.text.truncate(round_output_start);
                    outcome.usage = round_usage_start;
                    outcome.stop_reason = None;
                    round_text.clear();
                    current_tool = None;
                    current_tool_input.clear();
                    round_tool_calls.clear();
                    push_notification(
                        writer,
                        session_id,
                        json!({ "sessionUpdate": "retry_rollback" }),
                    )
                    .await;
                }
                Ok(_) => {}
                Err(e) => {
                    outcome.error = Some(format!("transport error: {e}"));
                    push_notification(
                        writer,
                        session_id,
                        json!({
                            "sessionUpdate": "error",
                            "message": format!("transport error: {e}"),
                        }),
                    )
                    .await;
                    break;
                }
            }
        }

        if outcome.error.is_some() || round_tool_calls.is_empty() {
            break;
        }
        let mut assistant_blocks = Vec::new();
        if !round_text.is_empty() {
            assistant_blocks.push(ContentBlock::Text {
                text: round_text,
                cache_control: None,
            });
        }
        assistant_blocks.extend(round_tool_calls.iter().map(ToolCall::to_tool_use_block));
        messages.push(Message {
            role: Role::Assistant,
            content: assistant_blocks,
            timestamp: Some(chrono::Utc::now()),
            tool_duration_ms: None,
        });

        let mut tool_result_blocks = Vec::with_capacity(round_tool_calls.len());
        for tool_call in round_tool_calls {
            let tool = tools.get(&tool_call.name);
            let run_result = match tool {
                Some(tool) => {
                    let decision =
                        if matches!(tool.permission(), mona_acp_tools::Permission::Required) {
                            permission
                                .request(&PermissionRequest {
                                    request_id: tool_call.id.clone(),
                                    tool_name: tool_call.name.clone(),
                                    input: tool_call.input.clone(),
                                })
                                .await
                        } else {
                            PermissionDecision::AllowOnce
                        };
                    match decision {
                        PermissionDecision::AllowOnce => {
                            tool.run(tool_call.input.clone(), cwd).await
                        }
                        PermissionDecision::RejectOnce => {
                            Err(mona_acp_tools::ToolError::PermissionDenied)
                        }
                    }
                }
                None => Err(mona_acp_tools::ToolError::Unknown(tool_call.name.clone())),
            };
            let (output_value, is_error) = match &run_result {
                Ok(output) => (output.output.clone(), output.is_error),
                Err(error) => (json!({"error": error.to_string()}), true),
            };
            push_notification(
                writer,
                session_id,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_call.id,
                    "status": if is_error { "failed" } else { "completed" },
                    "rawOutput": output_value,
                }),
            )
            .await;
            let output_text =
                serde_json::to_string(&output_value).unwrap_or_else(|_| output_value.to_string());
            tool_result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: tool_call.id,
                content: output_text,
                is_error: is_error.then_some(true),
            });
        }
        // Parallel tool calls must be answered by contiguous ToolResult
        // blocks in the very next user message (an Anthropic wire contract).
        messages.push(Message {
            role: Role::User,
            content: tool_result_blocks,
            timestamp: Some(chrono::Utc::now()),
            tool_duration_ms: None,
        });
        if round + 1 == MAX_TOOL_ROUNDS {
            return Err(anyhow::anyhow!(
                "tool round limit ({MAX_TOOL_ROUNDS}) reached"
            ));
        }
    }

    if let Some(err) = outcome.error.clone() {
        return Err(anyhow::anyhow!(err));
    }
    Ok(outcome)
}

/// Write one `session/update` push notification to the shared stdout
/// writer. Holds the writer mutex for the whole write so notification
/// ordering matches stream-event ordering.
async fn push_notification(writer: &SharedWriter, session_id: &str, update: Value) {
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": update,
        },
    });
    let line = match serde_json::to_string(&notification) {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "failed to serialize session/update notification");
            return;
        }
    };
    let mut w = writer.lock().await;
    if let Err(e) = async {
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok::<(), std::io::Error>(())
    }
    .await
    {
        error!(error = %e, "failed to write session/update notification");
    }
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

    if !session
        .handle
        .as_ref()
        .is_some_and(|handle| handle.auth.is_some())
    {
        return jsonrpc_error(
            id,
            -32002,
            "cannot change model before provider authentication",
        );
    }
    let updated = match state
        .sessions
        .reconfigure_provider(session_id, new_model, &session.effort)
    {
        Ok(session) => session,
        Err(error) => return jsonrpc_error(id, -32602, &error.to_string()),
    };
    info!(session_id, old_model = %session.model, new_model = %updated.model, "live model set");
    jsonrpc_result(
        id,
        json!({
            "model": updated.model,
            "effort": updated.effort,
            "applied": true,
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

    let session = match state.sessions.get(session_id) {
        Some(session) => session,
        None => return jsonrpc_error(id, -32004, &format!("session `{session_id}` not found")),
    };
    if !session
        .handle
        .as_ref()
        .is_some_and(|handle| handle.auth.is_some())
    {
        return jsonrpc_error(
            id,
            -32002,
            "cannot change reasoning effort before provider authentication",
        );
    }
    let updated = match state
        .sessions
        .reconfigure_provider(session_id, &session.model, effort)
    {
        Ok(session) => session,
        Err(error) => return jsonrpc_error(id, -32602, &error.to_string()),
    };
    info!(session_id, effort = %updated.effort, "live reasoning effort set");
    jsonrpc_result(
        id,
        json!({
            "model": updated.model,
            "effort": updated.effort,
            "applied": true,
        }),
    )
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

/// Extract the user prompt text from a `session/prompt` payload. Standard
/// ACP uses `prompt: [{type:"text", text:"..."}]`; older clients and
/// mona-acp's own smoke harness used a flat `text: "..."`. Both shapes
/// are accepted here. Empty string if neither shape is present.
fn extract_prompt_text(params: &Value) -> String {
    // Preferred: standard ACP `prompt` array. Take the first text block.
    if let Some(arr) = params.get("prompt").and_then(Value::as_array) {
        for block in arr {
            if let Some(obj) = block.as_object() {
                let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
                if kind == "text" {
                    if let Some(text) = obj.get("text").and_then(Value::as_str) {
                        return text.to_string();
                    }
                }
            }
        }
        // No text block found; return the JSON serialization so the
        // user at least sees what mona-acp got instead of silently
        // dropping it.
        return serde_json::to_string(arr).unwrap_or_default();
    }
    // Legacy: mona-acp's flat `text` string.
    if let Some(text) = params.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderHandle;
    use crate::provider_whitelist::SupportedProvider;
    use async_trait::async_trait;
    use futures::stream;
    use mona_message_types::{Message, StreamEvent, ToolDefinition};
    use mona_provider_core::{EventStream, Provider};
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Mutex as AsyncMutex;

    /// Mock provider that replays a scripted sequence of `StreamEvent`s.
    /// `complete()` returns a one-shot stream built from `events`; each
    /// call replaces the script. Tests use this to assert that
    /// `drive_provider_stream` accumulates text, pushes the right
    /// `session/update` notifications, and surfaces provider errors.
    struct MockProvider {
        // std::sync::Mutex so we can pop a script without holding an async
        // guard across an `await`. One script is consumed per completion
        // round, which lets tests exercise tool-result continuations.
        events: StdMutex<VecDeque<Vec<Result<StreamEvent>>>>,
        calls: StdMutex<Vec<Vec<Message>>>,
        tool_names: StdMutex<Vec<Vec<String>>>,
        model: String,
    }

    impl MockProvider {
        fn new(model: &str) -> Self {
            Self {
                events: StdMutex::new(VecDeque::new()),
                calls: StdMutex::new(Vec::new()),
                tool_names: StdMutex::new(Vec::new()),
                model: model.to_string(),
            }
        }
        fn script(&self, events: Vec<Result<StreamEvent>>) {
            self.events.lock().unwrap().push_back(events);
        }
        fn calls(&self) -> Vec<Vec<Message>> {
            self.calls.lock().unwrap().clone()
        }
        fn tool_names(&self) -> Vec<Vec<String>> {
            self.tool_names.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn complete(
            &self,
            messages: &[Message],
            tools: &[ToolDefinition],
            _system: &str,
            _resume_session_id: Option<&str>,
        ) -> Result<EventStream> {
            self.calls.lock().unwrap().push(messages.to_vec());
            self.tool_names
                .lock()
                .unwrap()
                .push(tools.iter().map(|tool| tool.name.clone()).collect());
            let events = self.events.lock().unwrap().pop_front().unwrap_or_default();
            let s: Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>> =
                Box::pin(stream::iter(events));
            Ok(s)
        }
        fn name(&self) -> &str {
            "mock"
        }
        fn model(&self) -> String {
            self.model.clone()
        }
        fn fork(&self) -> Arc<dyn Provider> {
            // The stream tests never fork, so just hand back a new
            // MockProvider with the same model and an empty script.
            Arc::new(MockProvider::new(&self.model))
        }
    }

    fn shared_writer() -> (SharedWriter, Arc<AsyncMutex<Vec<u8>>>) {
        let buf = Arc::new(AsyncMutex::new(Vec::new()));
        let writer: SharedWriter =
            Arc::new(AsyncMutex::new(BufWriter::new(Box::new(InMemoryWriter {
                inner: buf.clone(),
            }))));
        (writer, buf)
    }

    /// Minimal `AsyncWrite` that appends to a shared `Vec<u8>`. The
    /// tests never interleave writers so the sync `Mutex` is fine.
    struct InMemoryWriter {
        inner: Arc<AsyncMutex<Vec<u8>>>,
    }

    impl tokio::io::AsyncWrite for InMemoryWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            // InMemoryWriter is Unpin, so we can safely get &mut Self.
            let me = self.get_mut();
            let inner = me.inner.clone();
            // Tests don't interleave writers so try_lock always succeeds.
            let mut guard = inner.try_lock().expect("writer lock contended in test");
            guard.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    async fn read_lines(buf: Arc<AsyncMutex<Vec<u8>>>) -> Vec<Value> {
        let bytes = buf.lock().await.clone();
        let text = String::from_utf8(bytes).unwrap_or_default();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("valid json notification"))
            .collect()
    }

    fn mock_handle(model: &str) -> (Arc<MockProvider>, ProviderHandle) {
        let provider = Arc::new(MockProvider::new(model));
        let handle = ProviderHandle {
            provider: provider.clone() as Arc<dyn Provider>,
            auth: None,
            provider_kind: SupportedProvider::Codex,
        };
        (provider, handle)
    }

    #[tokio::test]
    async fn drive_provider_stream_accumulates_text_and_usage() {
        let (provider, handle) = mock_handle("gpt-5.5");
        provider.script(vec![
            Ok(StreamEvent::TextDelta("hello".into())),
            Ok(StreamEvent::TextDelta(" mona".into())),
            Ok(StreamEvent::TextDone),
            Ok(StreamEvent::TokenUsage {
                input_tokens: Some(11),
                output_tokens: Some(10),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            }),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ]);

        let (writer, buf) = shared_writer();
        let outcome = drive_provider_stream(
            &handle,
            &mona_acp_tools::default_registry(),
            &PathBuf::from("/tmp"),
            vec![Message::user("hi")],
            &writer,
            "sess-1",
            &AlwaysAllow,
        )
        .await
        .expect("stream ok");

        assert_eq!(outcome.text, "hello mona");
        assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(outcome.usage, Some((11, 10)));
        assert!(outcome.error.is_none());

        let lines = read_lines(buf).await;
        // 4 notification-bearing events (TextDone is a notification;
        // MessageEnd is NOT — it's only carried in the final response).
        assert_eq!(lines.len(), 4);
        let kinds: Vec<&str> = lines
            .iter()
            .map(|l| {
                l["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "agent_message_chunk",
                "agent_message_chunk",
                "agent_message_done",
                "usage_update",
            ]
        );
    }

    #[tokio::test]
    async fn drive_provider_stream_surfaces_provider_error() {
        let (provider, handle) = mock_handle("gpt-5.5");
        provider.script(vec![
            Ok(StreamEvent::TextDelta("partial".into())),
            Ok(StreamEvent::Error {
                message: "rate limited".into(),
                retry_after_secs: Some(30),
            }),
        ]);

        let (writer, buf) = shared_writer();
        let err = drive_provider_stream(
            &handle,
            &mona_acp_tools::default_registry(),
            &PathBuf::from("/tmp"),
            vec![Message::user("hi")],
            &writer,
            "sess-1",
            &AlwaysAllow,
        )
        .await
        .expect_err("stream should error");

        assert!(err.to_string().contains("rate limited"));

        let lines = read_lines(buf).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["params"]["update"]["sessionUpdate"], "error");
        assert_eq!(lines[1]["params"]["update"]["message"], "rate limited");
        assert_eq!(lines[1]["params"]["update"]["retryAfterSecs"], 30);
    }

    #[tokio::test]
    async fn drive_provider_stream_emits_thinking_and_tool_notifications() {
        let (provider, handle) = mock_handle("gpt-5.5");
        provider.script(vec![
            Ok(StreamEvent::ThinkingStart),
            Ok(StreamEvent::ThinkingDelta("reasoning...".into())),
            Ok(StreamEvent::ThinkingEnd),
            Ok(StreamEvent::ToolUseStart {
                id: "call-1".into(),
                name: "bash".into(),
            }),
            Ok(StreamEvent::ToolInputDelta("{\"command\":\"pwd\"}".into())),
            Ok(StreamEvent::ToolUseEnd),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("tool_use".into()),
            }),
        ]);
        provider.script(vec![Ok(StreamEvent::MessageEnd {
            stop_reason: Some("end_turn".into()),
        })]);

        let (writer, buf) = shared_writer();
        let outcome = drive_provider_stream(
            &handle,
            &mona_acp_tools::default_registry(),
            &PathBuf::from("/tmp"),
            vec![Message::user("list files")],
            &writer,
            "sess-1",
            &AlwaysAllow,
        )
        .await
        .expect("stream ok");

        assert_eq!(outcome.text, "");
        assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));

        let lines = read_lines(buf).await;
        let kinds: Vec<&str> = lines
            .iter()
            .map(|l| {
                l["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "agent_thought_chunk",
                "agent_thought_chunk",
                "agent_thought_chunk",
                "tool_call",
                "tool_call_update",
                "tool_call_update",
                "tool_call_update",
            ]
        );
        assert_eq!(
            lines[4]["params"]["update"]["rawInputDelta"],
            "{\"command\":\"pwd\"}"
        );
        assert_eq!(lines[5]["params"]["update"]["status"], "in_progress");
        assert_eq!(lines[6]["params"]["update"]["status"], "completed");
    }

    #[tokio::test]
    async fn drive_provider_stream_handles_empty_stream() {
        let (provider, handle) = mock_handle("gpt-5.5");
        provider.script(vec![]);

        let (writer, _buf) = shared_writer();
        let outcome = drive_provider_stream(
            &handle,
            &mona_acp_tools::default_registry(),
            &PathBuf::from("/tmp"),
            vec![Message::user("hi")],
            &writer,
            "sess-1",
            &AlwaysAllow,
        )
        .await
        .expect("empty stream is ok");

        assert_eq!(outcome.text, "");
        assert!(outcome.stop_reason.is_none());
        assert!(outcome.usage.is_none());
        assert!(outcome.error.is_none());
    }

    #[tokio::test]
    async fn acp_permission_round_trip_offers_only_one_time_choices() {
        let (writer, buf) = shared_writer();
        let pending: PendingClientResponses = Arc::new(AsyncMutex::new(HashMap::new()));
        let permission = Arc::new(AcpPermission::new(
            "session-1".into(),
            writer,
            pending.clone(),
            Arc::new(AtomicBool::new(false)),
        ));
        let request_task = {
            let permission = permission.clone();
            tokio::spawn(async move {
                let request = PermissionRequest {
                    request_id: "tool-1".into(),
                    tool_name: "bash".into(),
                    input: json!({"command":"pwd"}),
                };
                permission.request(&request).await
            })
        };

        timeout(Duration::from_secs(1), async {
            loop {
                if !buf.lock().await.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("permission request frame");

        let frames = read_lines(buf.clone()).await;
        assert_eq!(frames.len(), 1);
        let request = &frames[0];
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(request["params"]["sessionId"], "session-1");
        assert_eq!(request["params"]["toolCall"]["toolName"], "bash");
        let options = request["params"]["options"].as_array().unwrap();
        assert_eq!(options.len(), 2);
        assert!(options.iter().any(|option| option["kind"] == "allow_once"));
        assert!(options.iter().any(|option| option["kind"] == "reject_once"));
        assert!(
            !options
                .iter()
                .any(|option| option["kind"] == "allow_always")
        );
        let allow_id = options
            .iter()
            .find(|option| option["kind"] == "allow_once")
            .unwrap()["optionId"]
            .clone();
        assert!(
            deliver_client_response(
                &pending,
                json!({
                    "jsonrpc":"2.0",
                    "id": request["id"],
                    "result": {
                        "outcome": {"outcome":"selected", "optionId":allow_id}
                    }
                }),
            )
            .await
        );
        assert_eq!(request_task.await.unwrap(), PermissionDecision::AllowOnce);
        assert!(pending.lock().await.is_empty());

        let reject_task = {
            let permission = permission.clone();
            tokio::spawn(async move {
                let request = PermissionRequest {
                    request_id: "tool-2".into(),
                    tool_name: "write_file".into(),
                    input: json!({"path":"example.txt","content":"no"}),
                };
                permission.request(&request).await
            })
        };
        timeout(Duration::from_secs(1), async {
            loop {
                if read_lines(buf.clone()).await.len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second permission request frame");
        let frames = read_lines(buf).await;
        let request = &frames[1];
        let reject_id = request["params"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .find(|option| option["kind"] == "reject_once")
            .unwrap()["optionId"]
            .clone();
        assert!(
            deliver_client_response(
                &pending,
                json!({
                    "jsonrpc":"2.0",
                    "id": request["id"],
                    "result": {
                        "outcome": {"outcome":"selected", "optionId":reject_id}
                    }
                }),
            )
            .await
        );
        assert_eq!(reject_task.await.unwrap(), PermissionDecision::RejectOnce);
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn tool_round_trip_preserves_call_and_result_history() {
        let (provider, handle) = mock_handle("gpt-5.5");
        provider.script(vec![
            Ok(StreamEvent::ToolUseStart {
                id: "call-1".into(),
                name: "read_file".into(),
            }),
            Ok(StreamEvent::ToolInputDelta(
                "{\"path\":\"hello.txt\"}".into(),
            )),
            Ok(StreamEvent::ToolUseEnd),
            Ok(StreamEvent::ToolUseStart {
                id: "call-2".into(),
                name: "read_file".into(),
            }),
            Ok(StreamEvent::ToolInputDelta(
                "{\"path\":\"second.txt\"}".into(),
            )),
            Ok(StreamEvent::ToolUseEnd),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("tool_use".into()),
            }),
        ]);
        provider.script(vec![
            Ok(StreamEvent::TextDelta("The file says world.".into())),
            Ok(StreamEvent::TextDone),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ]);
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("hello.txt"), "world").unwrap();
        std::fs::write(temp.path().join("second.txt"), "again").unwrap();
        let (writer, buf) = shared_writer();
        let outcome = drive_provider_stream(
            &handle,
            &mona_acp_tools::default_registry(),
            &temp.path().to_path_buf(),
            vec![Message::user("read hello.txt")],
            &writer,
            "session-1",
            &AlwaysAllow,
        )
        .await
        .expect("tool continuation succeeds");

        assert_eq!(outcome.text, "The file says world.");
        assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
        let calls = provider.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].len(), 3);
        assert_eq!(calls[1][1].role, Role::Assistant);
        match &calls[1][1].content[0] {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "read_file");
                assert_eq!(input["path"], "hello.txt");
            }
            other => panic!("expected assistant ToolUse, got {other:?}"),
        }
        assert_eq!(calls[1][1].content.len(), 2);
        assert_eq!(calls[1][2].role, Role::User);
        match &calls[1][2].content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "call-1");
                assert!(content.contains("world"));
                assert_ne!(*is_error, Some(true));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        assert_eq!(calls[1][2].content.len(), 2);
        match &calls[1][2].content[1] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "call-2");
                assert!(content.contains("again"));
                assert_ne!(*is_error, Some(true));
            }
            other => panic!("expected parallel ToolResult, got {other:?}"),
        }
        assert_eq!(provider.tool_names().len(), 2);
        assert!(
            provider.tool_names()[0]
                .iter()
                .any(|name| name == "read_file")
        );
        let frames = read_lines(buf).await;
        let completed = frames
            .iter()
            .find(|frame| {
                frame["params"]["update"]["sessionUpdate"] == "tool_call_update"
                    && frame["params"]["update"]["status"] == "completed"
            })
            .expect("completed tool update");
        assert_eq!(
            completed["params"]["update"]["rawOutput"]["content"],
            "world"
        );
    }

    struct RejectAll;

    impl Permission for RejectAll {
        fn request<'a>(
            &'a self,
            _request: &'a PermissionRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PermissionDecision> + Send + 'a>>
        {
            Box::pin(async { PermissionDecision::RejectOnce })
        }
    }

    #[tokio::test]
    async fn rejected_tool_is_not_executed_and_model_receives_error_result() {
        let (provider, handle) = mock_handle("gpt-5.5");
        provider.script(vec![
            Ok(StreamEvent::ToolUseStart {
                id: "call-denied".into(),
                name: "bash".into(),
            }),
            Ok(StreamEvent::ToolInputDelta(
                "{\"command\":\"touch should-not-exist\"}".into(),
            )),
            Ok(StreamEvent::ToolUseEnd),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("tool_use".into()),
            }),
        ]);
        provider.script(vec![
            Ok(StreamEvent::TextDelta("The command was denied.".into())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ]);
        let temp = tempfile::TempDir::new().unwrap();
        let (writer, buf) = shared_writer();
        let outcome = drive_provider_stream(
            &handle,
            &mona_acp_tools::default_registry(),
            &temp.path().to_path_buf(),
            vec![Message::user("create a file")],
            &writer,
            "session-1",
            &RejectAll,
        )
        .await
        .expect("denial is returned to the model");

        assert_eq!(outcome.text, "The command was denied.");
        assert!(!temp.path().join("should-not-exist").exists());
        let calls = provider.calls();
        match &calls[1][2].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(content.contains("permission denied"));
                assert_eq!(*is_error, Some(true));
            }
            other => panic!("expected denied ToolResult, got {other:?}"),
        }
        let frames = read_lines(buf).await;
        assert!(frames.iter().any(|frame| {
            frame["params"]["update"]["status"] == "failed"
                && frame["params"]["update"]["rawOutput"]["error"]
                    .as_str()
                    .is_some_and(|message| message.contains("permission denied"))
        }));
    }

    #[tokio::test]
    async fn tool_loop_stops_after_eight_rounds() {
        let (provider, handle) = mock_handle("gpt-5.5");
        for index in 0..8 {
            provider.script(vec![
                Ok(StreamEvent::ToolUseStart {
                    id: format!("call-{index}"),
                    name: "ls".into(),
                }),
                Ok(StreamEvent::ToolInputDelta("{}".into())),
                Ok(StreamEvent::ToolUseEnd),
                Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("tool_use".into()),
                }),
            ]);
        }
        let temp = tempfile::TempDir::new().unwrap();
        let (writer, _buf) = shared_writer();
        let error = drive_provider_stream(
            &handle,
            &mona_acp_tools::default_registry(),
            &temp.path().to_path_buf(),
            vec![Message::user("keep listing")],
            &writer,
            "session-1",
            &AlwaysAllow,
        )
        .await
        .expect_err("tool loop must be bounded");
        assert!(error.to_string().contains("tool round limit (8)"));
        assert_eq!(provider.calls().len(), 8);
    }
}
