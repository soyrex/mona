//! `mona-jev` — Jev classifier for mona per-turn routing.
//!
//! The trait shape mirrors `src-tauri/src/model_router.rs:380` in the Monitter
//! desktop codebase (the original `JevClassifier` from which this is ported)
//! with two simplifications appropriate to the embedded-in-agent-loop use:
//!
//! 1. **No HTTP layer here.** Phase 2 ships with an in-memory mock classifier.
//!    Live HTTP calls to the Jev endpoint land in Phase 2.5.
//! 2. **No Keychain dependency.** The `live` classifier (Phase 2.5) will read
//!    the JEV API key from `std::env::var("MONA_JEV_API_KEY")` for non-macOS
//!    builds and from the macOS Keychain in production. Phase 2's mock needs
//!    neither.
//!
//! Every method and type here is `pub` so `crates/mona-acp/` can drive
//! per-turn classification without duplicating the trait.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub mod classifier;
pub mod mock;
pub mod safety;

pub use classifier::RuleBasedClassifier;
pub use mock::MockJevClassifier;
pub use safety::{SafetyVerdict, check_safety};

// ============================================================================
// Trait
// ============================================================================

/// Classifies a single agent turn and returns a [`JevRoutePlan`] describing
/// which model tier + reasoning effort the harness should use.
///
/// Implementations are expected to be cheap (sub-millisecond for the mock,
/// a single HTTP round-trip for the live classifier). They must NOT make
/// additional LLM calls; Jev is a separate decision endpoint, not an LLM.
#[async_trait]
pub trait JevClassifier: Send + Sync {
    async fn classify(&self, req: &JevClassifyRequest) -> Result<JevRoutePlan, JevError>;
}

// ============================================================================
// Request
// ============================================================================

/// One classification request.
///
/// `recent_messages` is the most recent N user/assistant message pairs in
/// the session (default N=4 in the harness). `last_turn_outcome` is the
/// outcome of the previous turn if any; the classifier uses this to detect
/// recovery patterns (e.g. a previous failure → use a stronger model).
#[derive(Debug, Clone)]
pub struct JevClassifyRequest {
    pub prompt: String,
    pub recent_messages: Vec<JevMessage>,
    pub last_turn_outcome: Option<JevTurnOutcome>,
    pub available_models: Vec<String>,
    pub available_efforts: Vec<String>,
}

/// One message in the recent conversation context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JevMessage {
    pub role: JevRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JevRole {
    User,
    Assistant,
    Tool,
}

/// Outcome of the previous turn. Drives recovery routing (escalation on
/// failure, de-escalation on success).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JevTurnOutcome {
    Passed,
    Failed { reason: String },
    Uncertain { reason: String },
}

// ============================================================================
// Plan
// ============================================================================

/// One classification result. The harness applies the tier + effort before
/// the next `provider.complete()` call.
///
/// Sensitive prompts (`sensitive: true`) are a HARD override: the harness
/// must short-circuit to human review regardless of tier/effort.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JevRoutePlan {
    pub tier: ModelTier,
    pub effort: Option<String>,
    pub reasoning_level: ReasoningLevel,
    pub execution_mode: ExecutionMode,
    pub permission_tier: PermissionTier,
    pub confidence: f32,
    pub rationale: String,
    pub sensitive: bool,
    pub trace_id: Uuid,
}

impl JevRoutePlan {
    /// A passthrough plan: "use whatever the session is currently on."
    /// Used when the caller has policy `Off` or wants to skip classification.
    pub fn passthrough(current_tier: ModelTier, current_effort: Option<String>) -> Self {
        Self {
            tier: current_tier,
            effort: current_effort,
            reasoning_level: ReasoningLevel::Standard,
            execution_mode: ExecutionMode::Autopilot,
            permission_tier: PermissionTier::Read,
            confidence: 1.0,
            rationale: "passthrough (no classification)".into(),
            sensitive: false,
            trace_id: Uuid::new_v4(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTier {
    /// Cheap, fast model. Examples: `gpt-5-mini`, `claude-haiku-4-5`.
    Fast,
    /// Mid-range. Examples: `gpt-5.4`, `claude-sonnet-4-6`.
    Balanced,
    /// Capable. Examples: `gpt-5.5`, `claude-opus-4-6`.
    Strong,
    /// Top of the line. Examples: `MiniMax-M3` (frontier), `claude-opus-5`.
    Frontier,
}

impl ModelTier {
    /// Map an abstract tier to a concrete model id from the agent's catalog.
    /// The operator owns the mapping in `agent.jev_model_tiers`; this is a
    /// fallback used when no mapping is configured.
    pub fn default_model(&self) -> &'static str {
        match self {
            Self::Fast => "gpt-5-mini",
            Self::Balanced => "gpt-5.4",
            Self::Strong => "gpt-5.5",
            Self::Frontier => "MiniMax-M3",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningLevel {
    Low,
    Standard,
    Deep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMode {
    /// Don't run; just produce a plan.
    Plan,
    /// Plan and ask for human confirmation before executing.
    Confirm,
    /// Execute without further confirmation.
    Autopilot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PermissionTier {
    /// Read-only operations: file reads, grep, etc.
    Read,
    /// Local writes: edit files in the working directory.
    WriteLocal,
    /// Remote writes: git push, network calls, etc.
    WriteRemote,
    /// Destructive: rm -rf, force push, etc.
    Destructive,
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum JevError {
    #[error("classification request was empty")]
    EmptyPrompt,
    #[error("classifier is offline: {0}")]
    Offline(String),
    #[error("safety check rejected the plan: {0}")]
    SafetyRejected(String),
    #[error("internal classifier error: {0}")]
    Internal(String),
}
