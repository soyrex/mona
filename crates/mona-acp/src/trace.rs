//! Per-turn router trace — JSON record of every routing decision.
//!
//! Persisted to `~/.mona/router-traces/{trace_id}.json`. The trace is the
//! audit log for what the per-turn Jev classifier decided, when, and
//! what changed as a result. Phase 3 Monitter UI will read this directory
//! to display the per-turn tier chip and the trace viewer.

use anyhow::{Context, Result};
use mona_jev::ModelTier;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterTrace {
    pub trace_id: Uuid,
    pub session_id: String,
    pub prompt_fingerprint: String,
    pub occurred_at: i64,
    pub trigger: TraceTrigger,
    pub proposed_tier: Option<ModelTier>,
    pub proposed_effort: Option<String>,
    pub applied: bool,
    pub confidence: f32,
    pub rationale: String,
    pub old_model: String,
    pub new_model: String,
    pub old_effort: String,
    pub new_effort: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum TraceTrigger {
    /// First prompt in a session.
    #[serde(rename = "initial_prompt")]
    InitialPrompt,
    /// Re-classified on a subsequent turn.
    #[serde(rename = "turn_reclassified")]
    TurnReclassified,
    /// Classifier unavailable; kept current model.
    #[serde(rename = "classifier_unavailable")]
    ClassifierUnavailable,
    /// User manually set the model via `session/set_model`.
    #[serde(rename = "user_override")]
    UserOverride,
    /// Cooldown expired; allowed a previously-deferred swap.
    #[serde(rename = "cooldown_expired")]
    CooldownExpired,
    /// Escalation followed from a previous failure.
    #[serde(rename = "escalation_followed")]
    EscalationFollowed,
}

impl TraceTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InitialPrompt => "initial_prompt",
            Self::TurnReclassified => "turn_reclassified",
            Self::ClassifierUnavailable => "classifier_unavailable",
            Self::UserOverride => "user_override",
            Self::CooldownExpired => "cooldown_expired",
            Self::EscalationFollowed => "escalation_followed",
        }
    }
}

/// Persist a single trace to `~/.mona/router-traces/{trace_id}.json`.
pub async fn persist(home_dir: &Path, trace: &RouterTrace) -> Result<()> {
    let dir = trace_dir(home_dir);
    fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("create router-traces dir {}", dir.display()))?;

    let path = dir.join(format!("{}.json", trace.trace_id));
    let json = serde_json::to_string_pretty(trace)?;
    fs::write(&path, json)
        .await
        .with_context(|| format!("write trace {}", path.display()))?;
    Ok(())
}

/// Compute the trace directory: `{home_dir}/router-traces`.
pub fn trace_dir(home_dir: &Path) -> PathBuf {
    home_dir.join("router-traces")
}

/// Convenience for tests: read the most recent trace for a session.
#[cfg(test)]
pub async fn read_last_trace(home_dir: &Path, session_id: &str) -> Result<Option<RouterTrace>> {
    let dir = trace_dir(home_dir);
    let mut entries = fs::read_dir(&dir).await?;
    let mut best: Option<(i64, RouterTrace)> = None;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_file() {
            let s = fs::read_to_string(entry.path()).await?;
            if let Ok(t) = serde_json::from_str::<RouterTrace>(&s) {
                if t.session_id == session_id {
                    if best.is_none() || t.occurred_at > best.as_ref().unwrap().0 {
                        best = Some((t.occurred_at, t));
                    }
                }
            }
        }
    }
    Ok(best.map(|(_, t)| t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn persist_and_read_back() {
        let tmp = tempdir();
        let trace = RouterTrace {
            trace_id: Uuid::new_v4(),
            session_id: "s1".into(),
            prompt_fingerprint: "abcdef0123456789".into(),
            occurred_at: 1_000_000,
            trigger: TraceTrigger::InitialPrompt,
            proposed_tier: Some(ModelTier::Balanced),
            proposed_effort: Some("medium".into()),
            applied: true,
            confidence: 0.7,
            rationale: "test".into(),
            old_model: "x".into(),
            new_model: "y".into(),
            old_effort: "low".into(),
            new_effort: "high".into(),
        };
        persist(&tmp, &trace).await.unwrap();
        let loaded = read_last_trace(&tmp, "s1").await.unwrap().unwrap();
        assert_eq!(loaded.trace_id, trace.trace_id);
        assert_eq!(loaded.session_id, "s1");
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("mona-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
