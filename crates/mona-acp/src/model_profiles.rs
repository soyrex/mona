//! Bounded, static model capability profiles for ACP routing surfaces.
//!
//! These are operator routing labels, not vendor capability claims.  The
//! input is the authenticated session catalogue; this module never expands it.

use mona_jev::ModelTier;
use serde::Serialize;

use crate::provider_whitelist::SupportedProvider;

const SOURCE_REFERENCE: &str =
    "crates/mona-acp/src/provider_whitelist.rs::SupportedProvider::model_for_tier";
const POLICY_LABEL: &str = "operator routing policy";

/// A small serializable profile suitable for bounded ACP state/UI metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProfile {
    pub schema_version: u8,
    pub provider: Option<SupportedProvider>,
    /// The exact identifier returned by the provider catalogue.
    pub model_id: String,
    /// A model may intentionally serve more than one abstract tier (for
    /// example MiniMax-M3 is both strong and frontier in the static policy).
    pub routing_tiers: Vec<ModelTier>,
    /// Labels are explicitly policy guidance, never vendor specifications.
    pub intended_task_strengths: Vec<String>,
    pub limitations: Vec<String>,
    pub source_reference: String,
    /// Vendor wording is included only for exact IDs with a reviewed source.
    pub vendor_summary: Option<String>,
    pub source_url: Option<String>,
    pub reviewed_at: Option<String>,
}

/// Build profiles strictly from `eligible_model_ids`.
///
/// Unknown catalogue entries are retained with no guessed tier or capability
/// claims.  The returned state is bounded by the caller's catalogue (and the
/// provider's four known tiers), with duplicates removed while preserving
/// catalogue order.
pub fn profiles_for_models(
    provider: SupportedProvider,
    eligible_model_ids: &[String],
) -> Vec<ModelProfile> {
    let mut profiles = Vec::new();
    for model_id in eligible_model_ids {
        if profiles
            .iter()
            .any(|profile: &ModelProfile| profile.model_id == *model_id)
        {
            continue;
        }
        let routing_tiers = known_tiers(provider, model_id);
        let (vendor_summary, source_url) = vendor_facts(model_id);
        let reviewed_at = source_url.as_ref().map(|_| "2026-09-22".to_owned());
        let (strengths, limitations) = policy_labels(&routing_tiers);
        profiles.push(ModelProfile {
            schema_version: 1,
            provider: Some(provider),
            model_id: model_id.clone(),
            routing_tiers,
            intended_task_strengths: strengths,
            limitations,
            source_reference: SOURCE_REFERENCE.to_owned(),
            vendor_summary,
            source_url,
            reviewed_at,
        });
    }
    profiles
}

/// Infer only exact static catalogue IDs; never interpret aliases or prefixes.
pub fn profiles_for_available_models(eligible_model_ids: &[String]) -> Vec<ModelProfile> {
    eligible_model_ids
        .iter()
        .map(|model_id| {
            SupportedProvider::all()
                .iter()
                .find_map(|provider| {
                    (!known_tiers(*provider, model_id).is_empty()).then(|| {
                        profiles_for_models(*provider, std::slice::from_ref(model_id))[0].clone()
                    })
                })
                .unwrap_or_else(|| unknown_profile(model_id))
        })
        .collect()
}

fn unknown_profile(model_id: &str) -> ModelProfile {
    let (_, limitations) = policy_labels(&[]);
    ModelProfile {
        schema_version: 1,
        provider: None,
        model_id: model_id.to_owned(),
        routing_tiers: Vec::new(),
        intended_task_strengths: Vec::new(),
        limitations,
        source_reference: SOURCE_REFERENCE.to_owned(),
        vendor_summary: None,
        source_url: None,
        reviewed_at: None,
    }
}

fn vendor_facts(model_id: &str) -> (Option<String>, Option<String>) {
    const OPENAI: &str = "https://developers.openai.com/api/docs/models/";
    const MINIMAX: &str = "https://platform.minimax.io/subscribe/token-plan";
    match model_id {
        "gpt-6-astra" => (
            Some("Most capable; intended for hardest end-to-end work.".into()),
            Some(format!("{OPENAI}gpt-6-astra.md")),
        ),
        "gpt-5.6-sol" => (
            Some("Intended for complex professional work.".into()),
            Some(format!("{OPENAI}gpt-5.6-sol.md")),
        ),
        "gpt-5.6-terra" => (
            Some("Balances intelligence and cost.".into()),
            Some(format!("{OPENAI}gpt-5.6-terra.md")),
        ),
        "gpt-5.6-luna" => (
            Some("Intended for cost-sensitive, high-volume workloads.".into()),
            Some(format!("{OPENAI}gpt-5.6-luna.md")),
        ),
        "MiniMax-M2.7-highspeed" | "MiniMax-M2.7" | "MiniMax-M3" => (
            Some("Model ID appears in the MiniMax product plan; no capability claim.".into()),
            Some(format!("{MINIMAX}?tab=api-enterprise")),
        ),
        _ => (None, None),
    }
}

fn known_tiers(provider: SupportedProvider, model_id: &str) -> Vec<ModelTier> {
    [
        ModelTier::Fast,
        ModelTier::Balanced,
        ModelTier::Strong,
        ModelTier::Frontier,
    ]
    .into_iter()
    .filter(|tier| provider.model_for_tier(*tier) == model_id)
    .collect()
}

fn policy_labels(tiers: &[ModelTier]) -> (Vec<String>, Vec<String>) {
    if tiers.is_empty() {
        return (
            Vec::new(),
            vec![format!(
                "{POLICY_LABEL}: unknown model capabilities must not be guessed"
            )],
        );
    }
    let strengths = tiers
        .iter()
        .map(|tier| {
            let tasks = match tier {
                ModelTier::Fast => "extraction, simple lookup, and narrow mechanical edits",
                ModelTier::Balanced => "ordinary bug fixes and contained feature implementation",
                ModelTier::Strong => {
                    "difficult debugging, multi-file refactoring, and careful review"
                }
                ModelTier::Frontier => {
                    "architecture, ambiguous cross-system reasoning, and complex end-to-end work"
                }
            };
            format!("{POLICY_LABEL}: {tasks}")
        })
        .collect();
    let limitations = vec![format!(
        "{POLICY_LABEL}: a short follow-up is not evidence that the task is simple; tier labels are heuristics"
    )];
    (strengths, limitations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_unavailable_models_and_preserves_exact_ids() {
        let eligible = vec!["gpt-5.6-sol".to_owned()];
        let profiles = profiles_for_models(SupportedProvider::Codex, &eligible);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].model_id, "gpt-5.6-sol");
        assert!(
            !profiles
                .iter()
                .any(|profile| profile.model_id == "gpt-6-astra")
        );
    }

    #[test]
    fn unknown_model_has_no_guessed_tier_or_specs() {
        let profiles = profiles_for_models(
            SupportedProvider::Claude,
            &["claude-future-unknown".to_owned()],
        );
        assert!(profiles[0].routing_tiers.is_empty());
        assert!(profiles[0].intended_task_strengths.is_empty());
        assert!(profiles[0].limitations[0].contains("must not be guessed"));
        assert_eq!(profiles[0].reviewed_at, None);
    }

    #[test]
    fn minimax_m3_maps_to_both_strong_and_frontier() {
        let profiles = profiles_for_models(SupportedProvider::Minimax, &["MiniMax-M3".to_owned()]);
        assert_eq!(
            profiles[0].routing_tiers,
            vec![ModelTier::Strong, ModelTier::Frontier]
        );
        assert!(
            profiles[0]
                .source_url
                .as_deref()
                .unwrap()
                .contains("api-enterprise")
        );
    }

    #[test]
    fn operator_labels_are_concrete_and_explicit() {
        let profiles = profiles_for_models(
            SupportedProvider::Codex,
            &["gpt-5.6-luna".to_owned(), "gpt-5.6-sol".to_owned()],
        );
        assert!(profiles[0].intended_task_strengths[0].contains("extraction"));
        assert!(profiles[1].intended_task_strengths[0].contains("difficult debugging"));
        assert!(profiles[0].limitations[0].starts_with("operator routing policy:"));
    }

    #[test]
    fn available_model_inference_is_exact_and_versioned() {
        let profiles = profiles_for_available_models(&[
            "gpt-6-astra".to_owned(),
            "gpt-6-astra-preview".to_owned(),
        ]);
        assert_eq!(profiles[0].schema_version, 1);
        assert_eq!(profiles[0].reviewed_at.as_deref(), Some("2026-09-22"));
        assert!(profiles[0].vendor_summary.is_some());
        assert!(profiles[1].provider.is_none());
        assert!(profiles[1].vendor_summary.is_none());
    }
}
