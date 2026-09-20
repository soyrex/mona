//! Safety gates — apply BEFORE applying any classification decision.
//!
//! Four hard rules. None of them are negotiable; violating any means the
//! harness should refuse to proceed.
//!
//! 1. **Sensitive prompt short-circuit.** If `plan.sensitive` is true, refuse
//!    to apply the tier change. Return an error so the caller routes the
//!    prompt to human review.
//!
//! 2. **Never widen permission tier.** If the current session permission tier
//!    is `Read`, the plan's permission tier cannot be `WriteLocal`,
//!    `WriteRemote`, or `Destructive`. Always downgrade; never upgrade.
//!
//! 3. **Cooldown.** If a tier swap happened within the last N turns, refuse
//!    this swap. Default N=2 (the harness config can override).
//!
//! 4. **Confidence floor.** If `plan.confidence` is below the floor
//!    (default 0.5), refuse the swap and keep the current model.
//!
//! These checks are pure functions; the harness holds the cooldown counter
//! because that's session state, not classifier state.

use crate::{JevRoutePlan, PermissionTier};

/// Result of applying all four safety gates to a proposed plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyVerdict {
    /// Plan is safe to apply.
    Apply,
    /// Plan was modified to be safe; apply the modified plan.
    Modified,
    /// Plan must not be applied; caller should keep current state.
    Refuse,
}

/// Configuration for the safety gates. Tunable per-agent.
#[derive(Debug, Clone)]
pub struct SafetyConfig {
    /// Minimum confidence to apply a plan. Below this, keep current model.
    pub confidence_floor: f32,
    /// Maximum permission tier the harness is allowed to widen to.
    /// Always at most `current`. Default = `current` (no widening).
    pub max_permission_tier_widening: PermissionTier,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            confidence_floor: 0.5,
            max_permission_tier_widening: PermissionTier::Read, // i.e. no widening allowed
        }
    }
}

/// Apply all four safety gates. Returns `(verdict, optionally_modified_plan)`.
///
/// `current_permission_tier` is the session's current tier; the plan may not
/// widen beyond it. `cooldown_active` is set by the harness when a swap
/// happened within the last N turns.
pub fn check_safety(
    plan: &JevRoutePlan,
    config: &SafetyConfig,
    current_permission_tier: PermissionTier,
    cooldown_active: bool,
) -> (SafetyVerdict, Option<JevRoutePlan>) {
    // Rule 1: sensitive prompt
    if plan.sensitive {
        return (SafetyVerdict::Refuse, None);
    }

    // Rule 4: confidence floor
    if plan.confidence < config.confidence_floor {
        return (SafetyVerdict::Refuse, None);
    }

    // Rule 3: cooldown
    if cooldown_active {
        return (SafetyVerdict::Refuse, None);
    }

    // Rule 2: never widen permission tier
    let mut modified = plan.clone();
    let mut was_modified = false;
    if modified.permission_tier > current_permission_tier
        && modified.permission_tier > config.max_permission_tier_widening
    {
        modified.permission_tier = current_permission_tier.min(config.max_permission_tier_widening);
        was_modified = true;
    }

    if was_modified {
        (SafetyVerdict::Modified, Some(modified))
    } else {
        (SafetyVerdict::Apply, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExecutionMode, ModelTier, ReasoningLevel};
    use uuid::Uuid;

    fn sample_plan() -> JevRoutePlan {
        JevRoutePlan {
            tier: ModelTier::Balanced,
            effort: Some("medium".into()),
            reasoning_level: ReasoningLevel::Standard,
            execution_mode: ExecutionMode::Autopilot,
            permission_tier: PermissionTier::WriteLocal,
            confidence: 0.8,
            rationale: "test".into(),
            sensitive: false,
            trace_id: Uuid::new_v4(),
        }
    }

    #[test]
    fn sensitive_plan_refused() {
        let mut plan = sample_plan();
        plan.sensitive = true;
        let (verdict, _) = check_safety(
            &plan,
            &SafetyConfig::default(),
            PermissionTier::WriteLocal,
            false,
        );
        assert_eq!(verdict, SafetyVerdict::Refuse);
    }

    #[test]
    fn low_confidence_refused() {
        let mut plan = sample_plan();
        plan.confidence = 0.3;
        let (verdict, _) = check_safety(
            &plan,
            &SafetyConfig::default(),
            PermissionTier::WriteLocal,
            false,
        );
        assert_eq!(verdict, SafetyVerdict::Refuse);
    }

    #[test]
    fn cooldown_refused() {
        let plan = sample_plan();
        let (verdict, _) = check_safety(
            &plan,
            &SafetyConfig::default(),
            PermissionTier::WriteLocal,
            true,
        );
        assert_eq!(verdict, SafetyVerdict::Refuse);
    }

    #[test]
    fn permission_widening_downgraded() {
        let mut plan = sample_plan();
        plan.permission_tier = PermissionTier::Destructive;
        let current = PermissionTier::Read;
        let (verdict, modified) = check_safety(
            &plan,
            &SafetyConfig::default(),
            current,
            false,
        );
        assert_eq!(verdict, SafetyVerdict::Modified);
        let m = modified.unwrap();
        assert_eq!(m.permission_tier, current);
    }

    #[test]
    fn permission_same_or_narrower_accepted() {
        let mut plan = sample_plan();
        plan.permission_tier = PermissionTier::Read;
        let (verdict, _) = check_safety(
            &plan,
            &SafetyConfig::default(),
            PermissionTier::WriteLocal,
            false,
        );
        assert_eq!(verdict, SafetyVerdict::Apply);
    }
}
