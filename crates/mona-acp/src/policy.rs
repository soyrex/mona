//! Jev routing policy — controls whether and how the per-turn classifier
//! runs.
//!
//! Maps to Monitter's `JevRoutingMode` in `src-tauri/src/model.rs`:
//! - `Off`: skip classification entirely; use whatever model the session is on.
//! - `Recommend`: classify, but only surface the recommendation; don't apply.
//! - `SafeAuto`: classify and apply, but never widen permission tier.
//! - `PerTurn`: classify and apply on every turn.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JevRoutePolicy {
    /// Skip classification. Use the session's current model.
    Off,
    /// Classify, surface the plan, but don't apply it.
    Recommend,
    /// Classify and apply. Never widen permission tier. (Phase 2 default.)
    #[default]
    SafeAuto,
    /// Classify and apply on every turn, no safety gate (advanced).
    PerTurn,
}

impl JevRoutePolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "never" => Some(Self::Off),
            "recommend" | "suggest" => Some(Self::Recommend),
            "safe_auto" | "safe-auto" | "safe" | "auto" => Some(Self::SafeAuto),
            "per_turn" | "per-turn" | "every-turn" => Some(Self::PerTurn),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Recommend => "recommend",
            Self::SafeAuto => "safe_auto",
            Self::PerTurn => "per_turn",
        }
    }

    /// Whether this policy applies the classifier's plan to the running model.
    pub fn applies_plan(&self) -> bool {
        matches!(self, Self::SafeAuto | Self::PerTurn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_known_policies() {
        assert_eq!(JevRoutePolicy::parse("off"), Some(JevRoutePolicy::Off));
        assert_eq!(
            JevRoutePolicy::parse("safe-auto"),
            Some(JevRoutePolicy::SafeAuto)
        );
        assert_eq!(
            JevRoutePolicy::parse("safe_auto"),
            Some(JevRoutePolicy::SafeAuto)
        );
        assert_eq!(
            JevRoutePolicy::parse("per_turn"),
            Some(JevRoutePolicy::PerTurn)
        );
        assert_eq!(JevRoutePolicy::parse("nonsense"), None);
    }

    #[test]
    fn applies_plan_distinguishes() {
        assert!(!JevRoutePolicy::Off.applies_plan());
        assert!(!JevRoutePolicy::Recommend.applies_plan());
        assert!(JevRoutePolicy::SafeAuto.applies_plan());
        assert!(JevRoutePolicy::PerTurn.applies_plan());
    }
}
