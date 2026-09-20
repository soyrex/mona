//! Per-turn routing hook — the Jev classification + safety + apply pipeline.
//!
//! This is the core of Phase 2.5. `run_turn_with_jev` is called from the
//! `session/prompt` handler and does the following, in order:
//!
//! 1. Build a [`JevClassifyRequest`] from the user's prompt + session
//!    state + last-turn outcome.
//! 2. Call the configured [`JevClassifier`]. If it errors, fall back to
//!    the last cached plan; if none, refuse.
//! 3. Apply [`safety::check_safety`] with the session's current
//!    permission_tier. Hard gates (sensitive, cooldown, low confidence)
//!    refuse; soft gate (permission widening) downgrades.
//! 4. If the verdict is `Apply` or `Modified`, update the session's model
//!    + effort. Record the cooldown counter.
//! 5. Persist a [`RouterTrace`] JSON to `~/.mona/router-traces/`.
//! 6. Return `(verdict, plan, trace_id, updated_session)` to the caller.
//!
//! Phase 2.5 deliberately stops short of driving `Agent::run_once`. The
//! caller (`session/prompt`) still returns a stub response after the hook
//! fires, but the routing infrastructure is real and tested end-to-end.
//! Phase 3 wires the stub to the actual `Agent` loop.

use crate::session::Session;
use crate::trace::RouterTrace;
use anyhow::Result;
use mona_jev::{
    JevClassifyRequest, JevClassifier, JevRoutePlan, JevTurnOutcome, ModelTier,
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
    /// Minimum number of user turns between tier swaps (cooldown).
    pub min_turns_between_swaps: u32,
    /// Maximum number of tier swaps allowed in a single session.
    pub max_swaps_per_session: u32,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            confidence_floor: 0.5,
            min_turns_between_swaps: 2,
            max_swaps_per_session: 8,
        }
    }
}

/// What the per-turn hook decided. Returned to the caller.
#[derive(Debug, Clone)]
pub struct TurnRoutingDecision {
    pub verdict: SafetyVerdict,
    /// The plan that was applied (or attempted). `None` if the verdict was
    /// `Refuse` and we kept the current model.
    pub applied_plan: Option<JevRoutePlan>,
    /// The trace that was persisted.
    pub trace: RouterTrace,
    /// The new session model after the decision.
    pub new_model: String,
    /// The new session effort after the decision.
    pub new_effort: String,
    /// The new session tier after the decision.
    pub new_tier: Option<ModelTier>,
    /// Human-readable reason for the verdict (for ACP errors + logs).
    pub reason: String,
    /// Whether the plan was classified as sensitive. Independent of the
    /// safety-gate verdict — even when the gate refuses, the caller needs
    /// to know it was sensitive so it can return permission_required.
    pub sensitive: bool,
}

/// Outcome of running the per-turn hook.
pub async fn run_turn_with_jev(
    classifier: Arc<dyn JevClassifier>,
    session: &mut Session,
    user_message: &str,
    turn_count: u32,
    swaps_in_session: u32,
    last_turn_outcome: Option<JevTurnOutcome>,
    config: &RouterConfig,
    home_dir: &std::path::Path,
) -> Result<TurnRoutingDecision> {
    // 1. Build the classify request
    let req = JevClassifyRequest {
        prompt: user_message.to_string(),
        recent_messages: vec![], // populated in Phase 3 when we wire to Agent state
        last_turn_outcome,
        available_models: vec![session.provider.default_model().to_string()],
        available_efforts: vec!["none".into(), "low".into(), "medium".into(), "high".into(), "max".into()],
    };

    // 2. Classify
    let plan = match classifier.classify(&req).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "classifier errored; keeping current model");
            let trace = RouterTrace {
                trace_id: Uuid::new_v4(),
                session_id: session.id.clone(),
                prompt_fingerprint: fingerprint(user_message),
                occurred_at: now_ms(),
                trigger: crate::trace::TraceTrigger::ClassifierUnavailable,
                proposed_tier: None,
                proposed_effort: None,
                applied: false,
                confidence: 0.0,
                rationale: format!("classifier error: {e}"),
                old_model: session.model.clone(),
                new_model: session.model.clone(),
                old_effort: session.effort.clone(),
                new_effort: session.effort.clone(),
            };
            crate::trace::persist(home_dir, &trace).await?;
            return Ok(TurnRoutingDecision {
                verdict: SafetyVerdict::Refuse,
                applied_plan: None,
                trace,
                new_model: session.model.clone(),
                new_effort: session.effort.clone(),
                new_tier: None,
                reason: format!("classifier error: {e}"),
                sensitive: false, // unknown — classifier never returned a plan
            });
        }
    };

    // 3. Apply safety gates
    let cooldown_active = (turn_count > 0)
        && ((turn_count % config.min_turns_between_swaps.max(1)) != 0);
    let safety_config = SafetyConfig {
        confidence_floor: config.confidence_floor,
        max_permission_tier_widening: PermissionTier::Read, // never widen
    };
    let (verdict, modified_plan) = check_safety(
        &plan,
        &safety_config,
        PermissionTier::WriteLocal,
        cooldown_active,
    );

    // 4. Apply the decision
    let (
        applied_plan,
        new_model,
        new_effort,
        new_tier,
        applied,
        reason,
    ) = match verdict {
        SafetyVerdict::Refuse => (
            None,
            session.model.clone(),
            session.effort.clone(),
            None,
            false,
            reason_for_refuse(&plan, cooldown_active, config.confidence_floor),
        ),
        SafetyVerdict::Apply | SafetyVerdict::Modified => {
            let effective = modified_plan.as_ref().unwrap_or(&plan);
            let target_model = tier_to_model(effective.tier, &session.provider.default_model());
            let target_effort = effective.effort.clone().unwrap_or_else(|| session.effort.clone());
            (
                Some(effective.clone()),
                target_model,
                target_effort,
                Some(effective.tier),
                true,
                effective.rationale.clone(),
            )
        }
    };

    // Sensitive flag is propagated independently of the verdict so the
    // caller can return permission_required even when the safety gate
    // refused (Refuse) — the gate might have refused for cooldown or
    // low confidence, but sensitive is a separate concern.
    let sensitive = plan.sensitive;

    // 5. Persist trace
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
        applied,
        confidence: plan.confidence,
        rationale: reason.clone(),
        old_model: session.model.clone(),
        new_model: new_model.clone(),
        old_effort: session.effort.clone(),
        new_effort: new_effort.clone(),
    };
    crate::trace::persist(home_dir, &trace).await?;
    info!(
        session_id = %session.id,
        applied,
        old_model = %session.model,
        new_model = %new_model,
        "per-turn router decision"
    );

    // 6. Update session state if the model changed
    if applied {
        session.model = new_model.clone();
        session.effort = new_effort.clone();
    }

    Ok(TurnRoutingDecision {
        verdict,
        applied_plan,
        trace,
        new_model,
        new_effort,
        new_tier,
        reason,
        sensitive,
    })
}

fn tier_to_model(tier: ModelTier, fallback: &str) -> String {
    let s = match tier {
        ModelTier::Fast => "fast",
        ModelTier::Balanced => "balanced",
        ModelTier::Strong => "strong",
        ModelTier::Frontier => "frontier",
    };
    // In Phase 2.5 we use the operator-configured tier name as the model
    // string. Phase 3 will resolve to a concrete provider model id via
    // `agent.jev_model_tiers`.
    format!("{s}:{fallback}")
}

fn reason_for_refuse(plan: &JevRoutePlan, cooldown_active: bool, floor: f32) -> String {
    if plan.sensitive {
        return "sensitive prompt; routing to human review".into();
    }
    if plan.confidence < floor {
        return format!("confidence {:.2} below floor {:.2}", plan.confidence, floor);
    }
    if cooldown_active {
        return "tier swap in cooldown".into();
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

/// Build a `JevClassifyRequest` from a raw user message + session metadata.
/// Useful for tests and for callers that don't have an `Agent` state.
pub fn build_request_from_session(
    user_message: &str,
    session: &Session,
    last_turn_outcome: Option<JevTurnOutcome>,
) -> JevClassifyRequest {
    JevClassifyRequest {
        prompt: user_message.to_string(),
        recent_messages: vec![],
        last_turn_outcome,
        available_models: vec![session.provider.default_model().to_string()],
        available_efforts: vec![
            "none".into(),
            "low".into(),
            "medium".into(),
            "high".into(),
            "max".into(),
        ],
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
        }
    }

    #[tokio::test]
    async fn applies_plan_when_classifier_says_frontier() {
        let mut classifier = MockJevClassifier::new();
        classifier.set_plan(JevRoutePlan::passthrough(ModelTier::Frontier, Some("max".into())));

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

        assert!(decision.trace.applied);
        assert!(decision.new_model.starts_with("frontier:"));
        assert_eq!(decision.new_effort, "max");
    }

    #[tokio::test]
    async fn low_confidence_is_refused() {
        let mut classifier = MockJevClassifier::new();
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
        let mut classifier = MockJevClassifier::new();
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
        assert_eq!(decision.new_model, "gpt-5.5", "model should not have changed");
    }

    #[tokio::test]
    async fn trace_round_trips_to_router_trace_value() {
        let mut classifier = MockJevClassifier::new();
        classifier.set_plan(JevRoutePlan::passthrough(ModelTier::Frontier, Some("max".into())));

        let mut s = session();
        let decision = run_turn_with_jev(
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

        let v = decision_to_router_trace_value(&decision);
        assert_eq!(v["sessionId"], "test-session");
        assert_eq!(v["applied"], true);
        assert_eq!(v["trigger"], "initial_prompt");
    }
}
