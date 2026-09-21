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
use futures::StreamExt;
use mona_jev::JevClassifier;
use mona_message_types::{Message, StreamEvent};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

/// Shared stdout writer for the ACP server. Handlers that stream updates
/// mid-turn (notably `session/prompt`) lock this for the duration of each
/// push so notification ordering is deterministic.
///
/// Wrapped in a boxed `AsyncWrite` trait object so tests can substitute an
/// in-memory buffer for `Stdout`.
pub type SharedWriter = Arc<Mutex<BufWriter<Box<dyn AsyncWrite + Send + Unpin>>>>;

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
    let writer: SharedWriter = Arc::new(Mutex::new(BufWriter::new(Box::new(stdout))));
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

        let response = handle_frame(line, &state, writer.clone()).await;
        let response_str = match serde_json::to_string(&response) {
            Ok(s) => s,
            Err(e) => {
                error!("failed to serialize response: {e}");
                continue;
            }
        };
        {
            let mut w = writer.lock().await;
            w.write_all(response_str.as_bytes()).await?;
            w.write_all(b"\n").await?;
            w.flush().await?;
        }
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

    match state.sessions.new_session(provider, model, effort, working_dir, &state.auth) {
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
/// Milestone B: drives a real model turn through the session's
/// `ProviderHandle`, streams `StreamEvent`s as `session/update` push
/// notifications, and returns the final assistant text plus token usage.
///
/// Scope:
/// - One user message per `session/prompt` call; no conversation history is
///   kept inside mona-acp yet.
/// - No tool round-trips (the provider's `ToolDefinition` slice is empty,
///   and `NativeToolCall` events are surfaced as `tool_call_update` so the
///   client can render them, but mona-acp does not execute them).
/// - `session/cancel` mid-stream is not yet wired; the turn runs to
///   completion or transport error.
/// - OAuth refresh during a turn surfaces as `unauthenticated`.
///
/// Phases 2.5 (per-turn Jev routing) and Phase 3 (auth loader) are
/// unchanged: routing still fires, the model is still swapped when the
/// classifier decides to, and the router trace is still emitted.
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
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");

    let mut session = match state.sessions.get(&session_id) {
        Some(s) => s,
        None => return jsonrpc_error(id, -32004, &format!("session `{session_id}` not found")),
    };

    // 1. Run the per-turn router (Phase 2.5 unchanged). Sensitive prompts
    //    short-circuit to permission_required before we touch the provider.
    let (turn_count_for_routing, swaps_in_session) = {
        let counters = state.session_counters.lock().await;
        let entry = counters.inner.get(&session_id).copied().unwrap_or((0, 0));
        (entry.0, entry.1)
    };
    {
        let mut counters = state.session_counters.lock().await;
        counters.get_or_insert(&session_id).0 += 1;
    }
    let decision = match run_turn_with_jev(
        state.classifier.clone(),
        &mut session,
        text,
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
    if decision.trace.applied {
        let mut counters = state.session_counters.lock().await;
        counters.record_swap(&session_id);
    }
    state.sessions.update(&session);
    *state.last_classification.lock().await = decision.applied_plan.clone();

    if decision.sensitive {
        return jsonrpc_error(
            id,
            -32001, // permission_required
            "sensitive prompt detected; routing to human review required",
        );
    }

    // 2. Drive the real provider. Sessions created without auth attach a
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
                    session_id, session.provider.as_str()
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

    // 3. Drive the real provider through the shared streaming loop.
    //    `drive_provider_stream` is the testable core; this handler just
    //    maps its outcome back to a JSON-RPC response.
    let outcome = match drive_provider_stream(
        &handle,
        vec![Message::user(text)],
        &writer,
        &session_id,
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

/// Open `Provider::complete` for the given messages and drain the stream,
/// emitting one `session/update` push notification per relevant event.
///
/// This is the testable core of `session/prompt`. It is `pub(crate)` so
/// the unit tests in `server::tests` can drive it directly with a mock
/// provider and an in-memory writer, without spinning up a child process
/// or polluting stdout.
pub(crate) async fn drive_provider_stream(
    handle: &crate::provider::ProviderHandle,
    messages: Vec<Message>,
    writer: &SharedWriter,
    session_id: &str,
) -> anyhow::Result<PromptOutcome> {
    debug!(
        session_id,
        provider = %handle.provider.name(),
        model = %handle.provider.model(),
        "drive_provider_stream opening provider.complete"
    );
    let mut stream = handle
        .provider
        .complete(&messages, &[], "", None)
        .await
        .map_err(|e| anyhow::anyhow!("provider `{}` open failed: {e}", handle.provider.name()))?;

    let mut outcome = PromptOutcome::default();
    let mut active_tool_id: Option<String> = None;

    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta(delta)) => {
                outcome.text.push_str(&delta);
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
                active_tool_id = Some(id.clone());
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
                let Some(tool_id) = active_tool_id.clone() else {
                    continue;
                };
                push_notification(
                    writer,
                    session_id,
                    json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": tool_id,
                        "rawInputDelta": delta,
                    }),
                )
                .await;
            }
            Ok(StreamEvent::ToolUseEnd) => {
                if let Some(tool_id) = active_tool_id.take() {
                    push_notification(
                        writer,
                        session_id,
                        json!({
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": tool_id,
                            "status": "in_progress",
                        }),
                    )
                    .await;
                }
            }
            Ok(StreamEvent::TokenUsage {
                input_tokens,
                output_tokens,
                ..
            }) => {
                if let (Some(i), Some(o)) = (input_tokens, output_tokens) {
                    outcome.usage = Some((i, o));
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
            Ok(StreamEvent::Error { message, retry_after_secs }) => {
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
                // The provider is about to retry from the top of the same
                // request; discard any partial output we accumulated.
                outcome.text.clear();
                push_notification(
                    writer,
                    session_id,
                    json!({ "sessionUpdate": "retry_rollback" }),
                )
                .await;
            }
            Ok(_) => {
                // Other event kinds (tool results, generated images,
                // compaction, session id, status detail, ...) are
                // intentionally not surfaced here. Monitter can read them
                // off `ProviderEvent` later if we extend the wire; for
                // Milestone B we keep the notification surface minimal.
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderHandle;
    use crate::provider_whitelist::SupportedProvider;
    use async_trait::async_trait;
    use futures::stream;
    use mona_message_types::{Message, StreamEvent, ToolDefinition};
    use mona_provider_core::{EventStream, Provider};
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Mutex as AsyncMutex;

    /// Mock provider that replays a scripted sequence of `StreamEvent`s.
    /// `complete()` returns a one-shot stream built from `events`; each
    /// call replaces the script. Tests use this to assert that
    /// `drive_provider_stream` accumulates text, pushes the right
    /// `session/update` notifications, and surfaces provider errors.
    struct MockProvider {
        // std::sync::Mutex so we can `take()` the script without holding
        // an async guard across an `await`. We rebuild the stream inline.
        events: StdMutex<Option<Vec<Result<StreamEvent>>>>,
        model: String,
    }

    impl MockProvider {
        fn new(model: &str) -> Self {
            Self {
                events: StdMutex::new(None),
                model: model.to_string(),
            }
        }
        fn script(&self, events: Vec<Result<StreamEvent>>) {
            *self.events.lock().unwrap() = Some(events);
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _system: &str,
            _resume_session_id: Option<&str>,
        ) -> Result<EventStream> {
            let events = self.events.lock().unwrap().take().unwrap_or_default();
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
        let writer: SharedWriter = Arc::new(AsyncMutex::new(BufWriter::new(Box::new(
            InMemoryWriter { inner: buf.clone() },
        ))));
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
            let mut guard = inner
                .try_lock()
                .expect("writer lock contended in test");
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
        let outcome =
            drive_provider_stream(&handle, vec![Message::user("hi")], &writer, "sess-1")
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
            .map(|l| l["params"]["update"]["sessionUpdate"].as_str().unwrap_or(""))
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
        let err = drive_provider_stream(&handle, vec![Message::user("hi")], &writer, "sess-1")
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
            Ok(StreamEvent::ToolInputDelta("{\"cmd\":\"ls\"}".into())),
            Ok(StreamEvent::ToolUseEnd),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("tool_use".into()),
            }),
        ]);

        let (writer, buf) = shared_writer();
        let outcome =
            drive_provider_stream(&handle, vec![Message::user("list files")], &writer, "sess-1")
                .await
                .expect("stream ok");

        assert_eq!(outcome.text, "");
        assert_eq!(outcome.stop_reason.as_deref(), Some("tool_use"));

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
            ]
        );
        assert_eq!(
            lines[4]["params"]["update"]["rawInputDelta"],
            "{\"cmd\":\"ls\"}"
        );
        assert_eq!(lines[5]["params"]["update"]["status"], "in_progress");
    }

    #[tokio::test]
    async fn drive_provider_stream_handles_empty_stream() {
        let (provider, handle) = mock_handle("gpt-5.5");
        provider.script(vec![]);

        let (writer, _buf) = shared_writer();
        let outcome =
            drive_provider_stream(&handle, vec![Message::user("hi")], &writer, "sess-1")
                .await
                .expect("empty stream is ok");

        assert_eq!(outcome.text, "");
        assert!(outcome.stop_reason.is_none());
        assert!(outcome.usage.is_none());
        assert!(outcome.error.is_none());
    }
}
