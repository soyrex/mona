//! Per-turn routing hook — the Jev classification + safety + apply pipeline.
//!
//! `run_turn_with_jev` is called from the
//! `session/prompt` handler and does the following, in order:
//!
//! 1. Build a [`JevClassifyRequest`] from the user's prompt + session
//!    state + last-turn outcome.
//! 2. Call the configured [`JevClassifier`]. If it errors, fall back to
//!    the last cached plan; if none, refuse.
//! 3. Apply [`safety::check_safety`] with the session's current
//!    permission_tier. Hard gates (sensitive, cooldown, low confidence)
//!    refuse; soft gate (permission widening) downgrades.
//! 4. Resolve the abstract tier to a concrete model within the session's
//!    existing provider.
//! 5. Return a pending decision to the caller. The caller applies it to an
//!    atomic provider candidate, then calls [`finalize_routing_decision`] to
//!    record the actual outcome and persist the trace.
//!
//! The caller now drives a real provider and bounded tool loop after this
//! hook. C.2 deliberately separates safety approval from runtime application
//! so a provider rejection cannot leave session metadata or traces claiming a
//! model switch that did not happen.

use crate::session::Session;
use crate::trace::RouterTrace;
use anyhow::Result;
use mona_jev::{
    JevClassifier, JevClassifyRequest, JevMessage, JevRoutePlan, JevTurnOutcome, ModelTier,
    PermissionTier, SafetyVerdict, check_safety, safety::SafetyConfig,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use uuid::Uuid;

/// Configuration for the per-turn router. Operator-tunable per agent.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// Minimum confidence to apply a plan. Below this, keep current model.
    pub confidence_floor: f32,
    /// Minimum number of user turns between tier swaps. Zero disables the
    /// fixed cooldown so every user turn may be independently routed.
    pub min_turns_between_swaps: u32,
    /// Maximum number of tier swaps allowed in a single session.
    pub max_swaps_per_session: u32,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            confidence_floor: 0.5,
            min_turns_between_swaps: 0,
            max_swaps_per_session: 8,
        }
    }
}

/// What the per-turn hook decided. Returned to the caller.
#[derive(Debug, Clone)]
pub struct TurnRoutingDecision {
    pub verdict: SafetyVerdict,
    /// The safety-approved plan awaiting live provider application. `None` if
    /// the verdict was `Refuse` and the current runtime must be kept.
    pub applied_plan: Option<JevRoutePlan>,
    /// Trace draft. The caller finalizes and persists it after attempting the
    /// live runtime change.
    pub trace: RouterTrace,
    /// The actual session model after finalization. Before finalization this
    /// remains the current model.
    pub new_model: String,
    /// The actual session effort after finalization. Before finalization this
    /// remains the current effort.
    pub new_effort: String,
    /// Concrete provider-scoped configuration requested by Jev. These remain
    /// visible even when safety refuses or runtime application rolls back.
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    /// The new session tier after the decision.
    pub new_tier: Option<ModelTier>,
    /// Human-readable reason for the verdict (for ACP errors + logs).
    pub reason: String,
    /// Whether the plan was classified as sensitive. Independent of the
    /// safety-gate verdict — even when the gate refuses, the caller needs
    /// to know it was sensitive so it can return permission_required.
    pub sensitive: bool,
}

/// Create an auditable no-classification decision. Used for the explicit
/// `off` policy; it must not invoke the classifier or invent a proposal.
pub fn skipped_routing_decision(
    session: &Session,
    user_message: &str,
    reason: &str,
) -> TurnRoutingDecision {
    let trace = RouterTrace {
        trace_id: Uuid::new_v4(),
        session_id: session.id.clone(),
        prompt_fingerprint: fingerprint(user_message),
        occurred_at: now_ms(),
        trigger: crate::trace::TraceTrigger::TurnReclassified,
        proposed_tier: None,
        proposed_effort: None,
        requested_model: None,
        requested_effort: None,
        applied: false,
        application_error: None,
        confidence: 1.0,
        rationale: reason.to_string(),
        old_model: session.model.clone(),
        new_model: session.model.clone(),
        old_effort: session.effort.clone(),
        new_effort: session.effort.clone(),
    };
    TurnRoutingDecision {
        verdict: SafetyVerdict::Refuse,
        applied_plan: None,
        trace,
        new_model: session.model.clone(),
        new_effort: session.effort.clone(),
        requested_model: None,
        requested_effort: None,
        new_tier: None,
        reason: reason.to_string(),
        sensitive: false,
    }
}

/// Outcome of running the per-turn hook.
pub async fn run_turn_with_jev(
    classifier: Arc<dyn JevClassifier>,
    session: &Session,
    user_message: &str,
    turn_count: u32,
    swaps_in_session: u32,
    last_turn_outcome: Option<JevTurnOutcome>,
    config: &RouterConfig,
    _home_dir: &std::path::Path,
) -> Result<TurnRoutingDecision> {
    // Compatibility entry point for embedders that only have a total turn
    // count. The ACP server supplies the more accurate per-session value
    // based on the last successful live swap.
    let cooldown_active = config.min_turns_between_swaps > 0
        && turn_count > 0
        && (turn_count % config.min_turns_between_swaps != 0);
    run_turn_with_jev_context(
        classifier,
        session,
        user_message,
        turn_count,
        swaps_in_session,
        Vec::new(),
        last_turn_outcome,
        None,
        cooldown_active,
        config,
        _home_dir,
    )
    .await
}

/// Context-aware router used by the ACP runtime. `cached_plan` is a
/// per-session last-known-good classifier result; it is only consulted when
/// classification itself fails, never to bypass the normal safety gates.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn_with_jev_context(
    classifier: Arc<dyn JevClassifier>,
    session: &Session,
    user_message: &str,
    turn_count: u32,
    swaps_in_session: u32,
    recent_messages: Vec<JevMessage>,
    last_turn_outcome: Option<JevTurnOutcome>,
    cached_plan: Option<JevRoutePlan>,
    cooldown_active: bool,
    config: &RouterConfig,
    _home_dir: &std::path::Path,
) -> Result<TurnRoutingDecision> {
    // 1. Build the classify request
    let req = build_request_from_session_with_context(
        user_message,
        session,
        recent_messages,
        last_turn_outcome,
    );

    // 2. Classify
    let plan = match classifier.classify(&req).await {
        Ok(p) => p,
        Err(e) => {
            if let Some(plan) = cached_plan {
                warn!(error = %e, "classifier errored; using cached plan through normal safety gates");
                plan
            } else {
                warn!(error = %e, "classifier errored; keeping current model");
                let trace = RouterTrace {
                    trace_id: Uuid::new_v4(),
                    session_id: session.id.clone(),
                    prompt_fingerprint: fingerprint(user_message),
                    occurred_at: now_ms(),
                    trigger: crate::trace::TraceTrigger::ClassifierUnavailable,
                    proposed_tier: None,
                    proposed_effort: None,
                    requested_model: None,
                    requested_effort: None,
                    applied: false,
                    application_error: None,
                    confidence: 0.0,
                    rationale: format!("classifier error: {e}"),
                    old_model: session.model.clone(),
                    new_model: session.model.clone(),
                    old_effort: session.effort.clone(),
                    new_effort: session.effort.clone(),
                };
                return Ok(TurnRoutingDecision {
                    verdict: SafetyVerdict::Refuse,
                    applied_plan: None,
                    trace,
                    new_model: session.model.clone(),
                    new_effort: session.effort.clone(),
                    requested_model: None,
                    requested_effort: None,
                    new_tier: None,
                    reason: format!("classifier error: {e}"),
                    sensitive: false, // unknown — classifier never returned a plan
                });
            }
        }
    };

    // 3. Apply safety gates
    let safety_config = SafetyConfig {
        confidence_floor: config.confidence_floor,
        max_permission_tier_widening: PermissionTier::Read, // never widen
    };
    let (mut verdict, modified_plan) = check_safety(
        &plan,
        &safety_config,
        PermissionTier::WriteLocal,
        cooldown_active,
    );
    let requested_plan = modified_plan.as_ref().unwrap_or(&plan);
    let requested_model = eligible_model_for_tier(session, requested_plan.tier);
    let requested_effort = requested_plan
        .effort
        .clone()
        .unwrap_or_else(|| session.effort.clone());
    let swap_budget_exhausted = swaps_in_session >= config.max_swaps_per_session
        && (requested_model != session.model || requested_effort != session.effort);
    if swap_budget_exhausted {
        verdict = SafetyVerdict::Refuse;
    }

    // 4. Return the safety-approved plan without mutating the session. The
    // caller applies it to an isolated provider candidate.
    let (applied_plan, new_tier, reason) = match verdict {
        SafetyVerdict::Refuse => (
            None,
            None,
            reason_for_refuse(
                &plan,
                cooldown_active,
                swap_budget_exhausted,
                config.confidence_floor,
            ),
        ),
        SafetyVerdict::Apply | SafetyVerdict::Modified => {
            let effective = modified_plan.as_ref().unwrap_or(&plan);
            (
                Some(effective.clone()),
                Some(effective.tier),
                effective.rationale.clone(),
            )
        }
    };

    // Sensitive flag is propagated independently of the verdict so the
    // caller can return permission_required even when the safety gate
    // refused (Refuse) — the gate might have refused for cooldown or
    // low confidence, but sensitive is a separate concern.
    let sensitive = plan.sensitive;

    // 5. Build a pending trace. `new_*` remain the actual current values until
    // the server calls `finalize_routing_decision`.
    let trace = RouterTrace {
        trace_id: plan.trace_id,
        session_id: session.id.clone(),
        prompt_fingerprint: fingerprint(user_message),
        occurred_at: now_ms(),
        trigger: if turn_count == 0 {
            crate::trace::TraceTrigger::InitialPrompt
        } else {
            crate::trace::TraceTrigger::TurnReclassified
        },
        proposed_tier: Some(plan.tier),
        proposed_effort: plan.effort.clone(),
        requested_model: Some(requested_model.clone()),
        requested_effort: Some(requested_effort.clone()),
        applied: false,
        application_error: None,
        confidence: plan.confidence,
        rationale: reason.clone(),
        old_model: session.model.clone(),
        new_model: session.model.clone(),
        old_effort: session.effort.clone(),
        new_effort: session.effort.clone(),
    };
    info!(
        session_id = %session.id,
        approved = applied_plan.is_some(),
        old_model = %session.model,
        requested_model,
        "per-turn router proposal"
    );

    Ok(TurnRoutingDecision {
        verdict,
        applied_plan,
        trace,
        new_model: session.model.clone(),
        new_effort: session.effort.clone(),
        requested_model: Some(requested_model),
        requested_effort: Some(requested_effort),
        new_tier,
        reason,
        sensitive,
    })
}

/// Record the actual live-provider outcome and persist the now-truthful trace.
/// `session` must be the successfully reconfigured session when `applied` is
/// true, or the unchanged pre-attempt session when false.
pub async fn finalize_routing_decision(
    decision: &mut TurnRoutingDecision,
    session: &Session,
    applied: bool,
    application_error: Option<String>,
    home_dir: &std::path::Path,
) -> Result<()> {
    let applied = applied && decision.applied_plan.is_some() && application_error.is_none();
    decision.new_model = session.model.clone();
    decision.new_effort = session.effort.clone();
    decision.trace.applied = applied;
    decision.trace.application_error = application_error.clone();
    decision.trace.new_model = session.model.clone();
    decision.trace.new_effort = session.effort.clone();
    if let Some(error) = application_error {
        decision.reason = format!(
            "{}; live provider application failed: {error}",
            decision.reason
        );
        decision.trace.rationale = decision.reason.clone();
    }
    crate::trace::persist(home_dir, &decision.trace).await?;
    info!(
        session_id = %session.id,
        applied,
        actual_model = %session.model,
        actual_effort = %session.effort,
        "per-turn router outcome"
    );
    Ok(())
}

fn reason_for_refuse(
    plan: &JevRoutePlan,
    cooldown_active: bool,
    swap_budget_exhausted: bool,
    floor: f32,
) -> String {
    if plan.sensitive {
        return "sensitive prompt; routing to human review".into();
    }
    if plan.confidence < floor {
        return format!("confidence {:.2} below floor {:.2}", plan.confidence, floor);
    }
    if cooldown_active {
        return "tier swap in cooldown".into();
    }
    if swap_budget_exhausted {
        return "session model-swap budget exhausted".into();
    }
    "unknown reason".into()
}

fn fingerprint(prompt: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    prompt.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Map a JSON tier string to `ModelTier`. Used by tests.
pub fn tier_from_str(s: &str) -> Option<ModelTier> {
    match s {
        "fast" => Some(ModelTier::Fast),
        "balanced" => Some(ModelTier::Balanced),
        "strong" => Some(ModelTier::Strong),
        "frontier" => Some(ModelTier::Frontier),
        _ => None,
    }
}

/// Resolve Jev's abstract quality tier through its known-model capability map,
/// then intersect that preference with the authenticated account catalogue.
/// A static known model can inform the ordering, but can never become a route
/// unless the provider advertised it for this session.
fn eligible_model_for_tier(session: &Session, requested: ModelTier) -> String {
    let eligible = if session.available_models.is_empty() {
        vec![session.model.clone()]
    } else {
        session.available_models.clone()
    };
    let tier_order: &[ModelTier] = match requested {
        ModelTier::Fast => &[
            ModelTier::Fast,
            ModelTier::Balanced,
            ModelTier::Strong,
            ModelTier::Frontier,
        ],
        ModelTier::Balanced => &[
            ModelTier::Balanced,
            ModelTier::Strong,
            ModelTier::Fast,
            ModelTier::Frontier,
        ],
        ModelTier::Strong => &[
            ModelTier::Strong,
            ModelTier::Frontier,
            ModelTier::Balanced,
            ModelTier::Fast,
        ],
        ModelTier::Frontier => &[
            ModelTier::Frontier,
            ModelTier::Strong,
            ModelTier::Balanced,
            ModelTier::Fast,
        ],
    };
    for tier in tier_order {
        let known = session.provider.model_for_tier(*tier);
        if let Some(model) = eligible
            .iter()
            .find(|model| model.eq_ignore_ascii_case(known))
        {
            return model.clone();
        }
    }
    eligible
        .iter()
        .find(|model| model.eq_ignore_ascii_case(&session.model))
        .cloned()
        .or_else(|| eligible.first().cloned())
        .unwrap_or_else(|| session.model.clone())
}

/// Build a `JevClassifyRequest` from a raw user message + session metadata.
/// Useful for tests and for callers that don't have an `Agent` state.
pub fn build_request_from_session(
    user_message: &str,
    session: &Session,
    last_turn_outcome: Option<JevTurnOutcome>,
) -> JevClassifyRequest {
    build_request_from_session_with_context(user_message, session, Vec::new(), last_turn_outcome)
}

/// Build a classifier request using the persisted, already bounded session
/// context. The caller owns truncation/persistence so this function remains
/// pure and useful in unit tests.
pub fn build_request_from_session_with_context(
    user_message: &str,
    session: &Session,
    recent_messages: Vec<JevMessage>,
    last_turn_outcome: Option<JevTurnOutcome>,
) -> JevClassifyRequest {
    let available_models = if session.available_models.is_empty() {
        vec![session.model.clone()]
    } else {
        session.available_models.clone()
    };
    let mut available_efforts = session
        .handle
        .as_ref()
        .map(|handle| {
            handle
                .provider
                .available_efforts()
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if available_efforts.is_empty() {
        available_efforts =
            if session.provider == crate::provider_whitelist::SupportedProvider::Minimax {
                vec!["none".into()]
            } else {
                vec![
                    "none".into(),
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "max".into(),
                ]
            };
    }
    JevClassifyRequest {
        prompt: user_message.to_string(),
        recent_messages,
        last_turn_outcome,
        available_models,
        available_efforts,
    }
}

/// Convert a TurnRoutingDecision to the JSON shape sent over the
/// `router_trace` push event on the ACP wire.
pub fn decision_to_router_trace_value(d: &TurnRoutingDecision) -> Value {
    let mut v = serde_json::json!({
        "traceId": d.trace.trace_id.to_string(),
        "sessionId": d.trace.session_id,
        "trigger": d.trace.trigger.as_str(),
        "applied": d.trace.applied,
        "confidence": d.trace.confidence,
        "rationale": d.trace.rationale,
        "oldModel": d.trace.old_model,
        "newModel": d.trace.new_model,
        "oldEffort": d.trace.old_effort,
        "newEffort": d.trace.new_effort,
        "occurredAt": d.trace.occurred_at,
        "promptFingerprint": d.trace.prompt_fingerprint,
    });
    if let Some(tier) = d.trace.proposed_tier {
        v["proposedTier"] = serde_json::json!(format!("{:?}", tier).to_lowercase());
    }
    if let Some(effort) = &d.trace.proposed_effort {
        v["proposedEffort"] = serde_json::json!(effort);
    }
    if let Some(model) = &d.trace.requested_model {
        v["requestedModel"] = serde_json::json!(model);
    }
    if let Some(effort) = &d.trace.requested_effort {
        v["requestedEffort"] = serde_json::json!(effort);
    }
    if let Some(error) = &d.trace.application_error {
        v["applicationError"] = serde_json::json!(error);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_whitelist::SupportedProvider;
    use crate::session::Session;
    use mona_jev::MockJevClassifier;

    fn session() -> Session {
        Session {
            id: "test-session".into(),
            provider: SupportedProvider::Codex,
            model: "gpt-5.5".into(),
            effort: "high".into(),
            working_dir: None,
            created_at: 0,
            permission_mode: crate::session::PermissionMode::Default,
            available_models: vec![
                "gpt-5.6-luna".into(),
                "gpt-5.6-terra".into(),
                "gpt-5.6-sol".into(),
                "gpt-6-astra".into(),
            ],
            handle: None,
        }
    }

    #[tokio::test]
    async fn proposes_concrete_provider_model_without_mutating_session() {
        let classifier = MockJevClassifier::new();
        classifier.set_plan(JevRoutePlan::passthrough(
            ModelTier::Frontier,
            Some("max".into()),
        ));

        let mut s = session();
        let decision = run_turn_with_jev(
            Arc::new(classifier),
            &mut s,
            "design the auth system",
            0,
            0,
            None,
            &RouterConfig::default(),
            &std::path::PathBuf::from("/tmp/mona-test"),
        )
        .await
        .unwrap();

        assert!(!decision.trace.applied, "runtime has not applied it yet");
        assert!(decision.applied_plan.is_some());
        assert_eq!(decision.requested_model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(decision.requested_effort.as_deref(), Some("max"));
        assert_eq!(decision.new_model, "gpt-5.5");
        assert_eq!(decision.new_effort, "high");
        assert_eq!(s.model, "gpt-5.5");
    }

    #[tokio::test]
    async fn unavailable_known_model_is_never_requested() {
        let classifier = MockJevClassifier::new();
        classifier.set_plan(JevRoutePlan::passthrough(
            ModelTier::Balanced,
            Some("medium".into()),
        ));

        let mut s = session();
        s.available_models = vec!["gpt-5.6-luna".into(), "gpt-5.6-sol".into()];
        let decision = run_turn_with_jev(
            Arc::new(classifier),
            &s,
            "implement this change",
            0,
            0,
            None,
            &RouterConfig::default(),
            &std::path::PathBuf::from("/tmp/mona-test"),
        )
        .await
        .unwrap();

        assert_eq!(decision.requested_model.as_deref(), Some("gpt-5.6-sol"));
        assert_ne!(decision.requested_model.as_deref(), Some("gpt-5.6-terra"));
    }

    #[test]
    fn classifier_sees_only_authenticated_models() {
        let mut s = session();
        s.available_models = vec!["gpt-5.6-luna".into(), "gpt-6-astra".into()];

        let request = build_request_from_session("hello", &s, None);

        assert_eq!(request.available_models, s.available_models);
        assert!(
            !request
                .available_models
                .contains(&"gpt-5.1-codex-mini".into())
        );
    }

    #[tokio::test]
    async fn low_confidence_is_refused() {
        let classifier = MockJevClassifier::new();
        let mut plan = JevRoutePlan::passthrough(ModelTier::Strong, Some("high".into()));
        plan.confidence = 0.3;
        classifier.set_plan(plan);

        let mut s = session();
        let decision = run_turn_with_jev(
            Arc::new(classifier),
            &mut s,
            "something",
            0,
            0,
            None,
            &RouterConfig::default(),
            &std::path::PathBuf::from("/tmp/mona-test"),
        )
        .await
        .unwrap();

        assert!(!decision.trace.applied);
        assert_eq!(decision.new_model, "gpt-5.5");
    }

    #[tokio::test]
    async fn cooldown_refuses_subsequent_swap() {
        // The per-turn cooldown is enforced inside `run_turn_with_jev` via
        // `safety::check_safety`. The unit tests in `mona-jev/src/safety.rs`
        // already cover the safety gates exhaustively (sensitive, low
        // confidence, cooldown, permission widening). This test confirms
        // the integration: at turn 1 with min_turns_between_swaps=2, the
        // hook reports `applied: false` even when the classifier says
        // Strong.
        let classifier = MockJevClassifier::new();
        let mut plan = JevRoutePlan::passthrough(ModelTier::Strong, Some("high".into()));
        plan.confidence = 0.8;
        classifier.set_plan(plan);

        let mut s = session();
        let config = RouterConfig {
            min_turns_between_swaps: 2,
            ..RouterConfig::default()
        };

        let decision = run_turn_with_jev(
            Arc::new(classifier),
            &mut s,
            "msg1",
            1, // turn_count = 1 → cooldown_active
            0,
            None,
            &config,
            &std::path::PathBuf::from("/tmp/mona-test"),
        )
        .await
        .unwrap();

        assert!(!decision.trace.applied, "second turn should be in cooldown");
        assert_eq!(
            decision.new_model, "gpt-5.5",
            "model should not have changed"
        );
    }

    #[tokio::test]
    async fn trace_round_trips_to_router_trace_value() {
        let classifier = MockJevClassifier::new();
        classifier.set_plan(JevRoutePlan::passthrough(
            ModelTier::Frontier,
            Some("max".into()),
        ));

        let mut s = session();
        let mut decision = run_turn_with_jev(
            Arc::new(classifier),
            &mut s,
            "test",
            0,
            0,
            None,
            &RouterConfig::default(),
            &std::path::PathBuf::from("/tmp/mona-test"),
        )
        .await
        .unwrap();

        s.model = decision.requested_model.clone().unwrap();
        s.effort = decision.requested_effort.clone().unwrap();
        let home = std::env::temp_dir().join(format!("mona-c2-{}", Uuid::new_v4()));
        finalize_routing_decision(&mut decision, &s, true, None, &home)
            .await
            .unwrap();

        let v = decision_to_router_trace_value(&decision);
        assert_eq!(v["sessionId"], "test-session");
        assert_eq!(v["applied"], true);
        assert_eq!(v["trigger"], "initial_prompt");
        assert_eq!(v["requestedModel"], "gpt-6-astra");
        assert_eq!(v["newModel"], "gpt-6-astra");
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn swap_budget_refuses_without_mutating_session() {
        let classifier = MockJevClassifier::new();
        classifier.set_plan(JevRoutePlan::passthrough(
            ModelTier::Frontier,
            Some("max".into()),
        ));
        let mut s = session();
        let config = RouterConfig {
            max_swaps_per_session: 1,
            ..RouterConfig::default()
        };
        let decision = run_turn_with_jev(
            Arc::new(classifier),
            &mut s,
            "another escalation",
            2,
            1,
            None,
            &config,
            std::path::Path::new("/tmp/mona-test"),
        )
        .await
        .unwrap();

        assert!(decision.applied_plan.is_none());
        assert!(!decision.trace.applied);
        assert!(decision.reason.contains("budget"));
        assert_eq!(s.model, "gpt-5.5");
    }

    #[tokio::test]
    async fn failed_runtime_application_records_requested_and_actual_values() {
        let classifier = MockJevClassifier::new();
        classifier.set_plan(JevRoutePlan::passthrough(
            ModelTier::Frontier,
            Some("max".into()),
        ));
        let mut s = session();
        let mut decision = run_turn_with_jev(
            Arc::new(classifier),
            &mut s,
            "escalate this",
            0,
            0,
            None,
            &RouterConfig::default(),
            std::path::Path::new("/tmp/mona-test"),
        )
        .await
        .unwrap();
        let home = std::env::temp_dir().join(format!("mona-c2-{}", Uuid::new_v4()));
        finalize_routing_decision(
            &mut decision,
            &s,
            false,
            Some("model unavailable".into()),
            &home,
        )
        .await
        .unwrap();

        assert!(!decision.trace.applied);
        assert_eq!(
            decision.trace.requested_model.as_deref(),
            Some("gpt-6-astra")
        );
        assert_eq!(decision.trace.new_model, "gpt-5.5");
        assert_eq!(
            decision.trace.application_error.as_deref(),
            Some("model unavailable")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn context_aware_router_passes_recent_messages_and_last_outcome() {
        let classifier = Arc::new(MockJevClassifier::new());
        let prior = vec![JevMessage {
            role: mona_jev::JevRole::Assistant,
            content: "previous answer".into(),
        }];
        let outcome = Some(JevTurnOutcome::Failed {
            reason: "provider stopped".into(),
        });
        run_turn_with_jev_context(
            classifier.clone(),
            &session(),
            "retry this",
            0,
            0,
            prior.clone(),
            outcome.clone(),
            None,
            true,
            &RouterConfig::default(),
            std::path::Path::new("/tmp/mona-test"),
        )
        .await
        .unwrap();
        let request = classifier.last_request().expect("classification request");
        assert_eq!(request.recent_messages[0].content, prior[0].content);
        assert!(matches!(
            request.last_turn_outcome,
            Some(JevTurnOutcome::Failed { .. })
        ));
    }
}
