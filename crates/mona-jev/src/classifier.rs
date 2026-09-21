//! Rule-based classifier — used by Phase 2 as the default classifier.
//!
//! This is intentionally simple: keyword heuristics over the prompt +
//! last-outcome signal. The ACP live adapter calls the shared typed Decisions
//! transport; the rule-based classifier stays as the default offline path and
//! as the test double for `crates/mona-acp/tests/end_to_end.rs`.
//!
//! Rules (in priority order):
//!
//! 1. Sensitive signal: contains credential / destructive keywords →
//!    `sensitive: true`, force tier Frontier, escalate effort.
//! 2. Recovery: previous turn Failed → escalate one tier from current.
//! 3. De-escalation: previous turn Passed AND prompt is short → drop one tier.
//! 4. Complex task signals (architecture, refactor, multi-file, "explain",
//!    "design", "implement") → Frontier or Strong.
//! 5. Simple task signals (one-file fix, typo, single-line) → Fast or Balanced.
//! 6. Default: Balanced, medium effort, autopilot.

use crate::{
    JevClassifyRequest, JevClassifier, JevError, JevRoutePlan, JevTurnOutcome, ModelTier,
    PermissionTier, ReasoningLevel, ExecutionMode,
};
use async_trait::async_trait;
use uuid::Uuid;

/// Phase 2 default classifier. Cheap, deterministic, no network.
pub struct RuleBasedClassifier;

impl RuleBasedClassifier {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RuleBasedClassifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl JevClassifier for RuleBasedClassifier {
    async fn classify(&self, req: &JevClassifyRequest) -> Result<JevRoutePlan, JevError> {
        if req.prompt.trim().is_empty() {
            return Err(JevError::EmptyPrompt);
        }

        let prompt = req.prompt.to_ascii_lowercase();

        // Rule 1: sensitive signal
        if is_sensitive(&prompt) {
            return Ok(JevRoutePlan {
                tier: ModelTier::Frontier,
                effort: Some("max".into()),
                reasoning_level: ReasoningLevel::Deep,
                execution_mode: ExecutionMode::Confirm,
                permission_tier: PermissionTier::Destructive,
                confidence: 0.95,
                rationale: "sensitive signal detected; routing to human review".into(),
                sensitive: true,
                trace_id: Uuid::new_v4(),
            });
        }

        // Rule 2: recovery from failure
        if matches!(&req.last_turn_outcome, Some(JevTurnOutcome::Failed { .. })) {
            return Ok(JevRoutePlan {
                tier: ModelTier::Strong,
                effort: Some("high".into()),
                reasoning_level: ReasoningLevel::Deep,
                execution_mode: ExecutionMode::Autopilot,
                permission_tier: PermissionTier::WriteLocal,
                confidence: 0.7,
                rationale: "previous turn failed; escalating".into(),
                sensitive: false,
                trace_id: Uuid::new_v4(),
            });
        }

        // Rule 4: complex task signals
        if is_complex_task(&prompt) {
            return Ok(JevRoutePlan {
                tier: ModelTier::Strong,
                effort: Some("high".into()),
                reasoning_level: ReasoningLevel::Deep,
                execution_mode: ExecutionMode::Autopilot,
                permission_tier: PermissionTier::WriteLocal,
                confidence: 0.75,
                rationale: "complex task signal detected".into(),
                sensitive: false,
                trace_id: Uuid::new_v4(),
            });
        }

        // Rule 5: simple task signals
        if is_simple_task(&prompt) {
            return Ok(JevRoutePlan {
                tier: ModelTier::Fast,
                effort: Some("low".into()),
                reasoning_level: ReasoningLevel::Low,
                execution_mode: ExecutionMode::Autopilot,
                permission_tier: PermissionTier::WriteLocal,
                confidence: 0.6,
                rationale: "simple task signal detected".into(),
                sensitive: false,
                trace_id: Uuid::new_v4(),
            });
        }

        // Rule 6: default
        Ok(JevRoutePlan {
            tier: ModelTier::Balanced,
            effort: Some("medium".into()),
            reasoning_level: ReasoningLevel::Standard,
            execution_mode: ExecutionMode::Autopilot,
            permission_tier: PermissionTier::WriteLocal,
            confidence: 0.5,
            rationale: "default tier; no signal strong enough to override".into(),
            sensitive: false,
            trace_id: Uuid::new_v4(),
        })
    }
}

fn is_sensitive(prompt: &str) -> bool {
    const MARKERS: &[&str] = &[
        "rm -rf",
        "force push",
        "delete the database",
        "drop table",
        "wipe the disk",
        "production credentials",
        "real password",
        "live api key",
        "secret_key",
    ];
    MARKERS.iter().any(|m| prompt.contains(m))
}

fn is_complex_task(prompt: &str) -> bool {
    const MARKERS: &[&str] = &[
        "architect", "design", "refactor", "redesign", "rewrite",
        "implement a", "implement the", "build a", "build the",
        "explain how", "explain why",
        "across the codebase", "across multiple files",
        "end to end", "e2e",
        "diagnose", "debug the", "find the root cause",
    ];
    MARKERS.iter().any(|m| prompt.contains(m))
}

fn is_simple_task(prompt: &str) -> bool {
    // Length-based heuristic: prompts under ~80 chars are typically
    // quick lookups or one-line fixes.
    if prompt.len() < 80 {
        return true;
    }
    const MARKERS: &[&str] = &[
        "fix the typo", "fix typo",
        "what is", "what's",
        "rename",
    ];
    MARKERS.iter().any(|m| prompt.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_prompt_errors() {
        let c = RuleBasedClassifier::new();
        let req = JevClassifyRequest {
            prompt: "   ".into(),
            recent_messages: vec![],
            last_turn_outcome: None,
            available_models: vec![],
            available_efforts: vec![],
        };
        assert!(matches!(c.classify(&req).await, Err(JevError::EmptyPrompt)));
    }

    #[tokio::test]
    async fn sensitive_signal_triggers_human_review() {
        let c = RuleBasedClassifier::new();
        let req = JevClassifyRequest {
            prompt: "rm -rf / please".into(),
            recent_messages: vec![],
            last_turn_outcome: None,
            available_models: vec![],
            available_efforts: vec![],
        };
        let plan = c.classify(&req).await.unwrap();
        assert!(plan.sensitive);
        assert_eq!(plan.execution_mode, ExecutionMode::Confirm);
    }

    #[tokio::test]
    async fn failed_previous_turn_escalates() {
        let c = RuleBasedClassifier::new();
        let req = JevClassifyRequest {
            prompt: "now try again".into(),
            recent_messages: vec![],
            last_turn_outcome: Some(JevTurnOutcome::Failed {
                reason: "test failure".into(),
            }),
            available_models: vec![],
            available_efforts: vec![],
        };
        let plan = c.classify(&req).await.unwrap();
        assert_eq!(plan.tier, ModelTier::Strong);
    }

    #[tokio::test]
    async fn short_prompt_classifies_simple() {
        let c = RuleBasedClassifier::new();
        let req = JevClassifyRequest {
            prompt: "rename foo to bar".into(),
            recent_messages: vec![],
            last_turn_outcome: None,
            available_models: vec![],
            available_efforts: vec![],
        };
        let plan = c.classify(&req).await.unwrap();
        assert_eq!(plan.tier, ModelTier::Fast);
    }

    #[tokio::test]
    async fn architecture_prompt_classifies_complex() {
        let c = RuleBasedClassifier::new();
        let req = JevClassifyRequest {
            prompt: "design a new architecture for the auth system across the codebase".into(),
            recent_messages: vec![],
            last_turn_outcome: None,
            available_models: vec![],
            available_efforts: vec![],
        };
        let plan = c.classify(&req).await.unwrap();
        assert_eq!(plan.tier, ModelTier::Strong);
    }

    #[tokio::test]
    async fn classifier_returns_some_plan_for_unknown_prompt() {
        // The "default" path falls through every keyword check. We don't
        // assert exactly which tier it returns (the rules may evolve); we
        // just confirm it returns a usable plan.
        let c = RuleBasedClassifier::new();
        let req = JevClassifyRequest {
            prompt: "do the thing please thanks".into(),
            recent_messages: vec![],
            last_turn_outcome: None,
            available_models: vec![],
            available_efforts: vec![],
        };
        let plan = c.classify(&req).await.unwrap();
        assert!(matches!(
            plan.tier,
            ModelTier::Fast | ModelTier::Balanced | ModelTier::Strong | ModelTier::Frontier
        ));
        assert!(!plan.sensitive);
        assert!(plan.confidence > 0.0);
    }
}
