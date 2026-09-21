//! Provider whitelist for `mona-acp`.
//!
//! Phase 2 supports exactly three providers:
//!
//! - `codex` (OpenAI)
//! - `claude` (Anthropic)
//! - `minimax` (MiniMax)
//!
//! Anything else is rejected with a clear error message. The full provider
//! matrix (OpenRouter, Bedrock, Copilot, Gemini, etc.) lands in Phase 4.

use serde::{Deserialize, Serialize};

/// The three providers Phase 2 supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SupportedProvider {
    Codex,
    Claude,
    Minimax,
}

impl SupportedProvider {
    /// Default model id for this provider when no override is given.
    pub fn default_model(&self) -> &'static str {
        match self {
            Self::Codex => "gpt-5.5",
            Self::Claude => "claude-sonnet-4-6",
            Self::Minimax => "MiniMax-M3",
        }
    }

    /// Default reasoning effort for this provider.
    pub fn default_effort(&self) -> &'static str {
        match self {
            Self::Codex | Self::Claude | Self::Minimax => "high",
        }
    }

    /// Stable provider string sent over the ACP wire (matches Monitter's
    /// `agent.jev_model_tiers` keys).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Minimax => "minimax",
        }
    }

    /// All supported providers, in display order.
    pub fn all() -> &'static [SupportedProvider] {
        &[Self::Codex, Self::Claude, Self::Minimax]
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error(
        "provider '{requested}' is not supported in mona Phase 2. Supported providers: codex, claude, minimax. See docs/PHASE-2-MONA-ACP.md for the full provider plan."
    )]
    Unsupported { requested: String },
}

/// Parse a provider string into a `SupportedProvider`. Accepts aliases:
/// `openai` and `gpt-*` map to `Codex`; `anthropic` and `sonnet*|opus*|haiku*`
/// map to `Claude`; the rest map to `Minimax`.
///
/// `"auto"` is the standard ACP form for "the harness picks". mona-acp
/// treats `auto` as an unresolved provider so the caller can substitute the
/// first configured provider via `parse_provider_with_default`.
pub fn parse_provider(s: &str) -> Result<SupportedProvider, ProviderError> {
    let normalized = s.trim().to_ascii_lowercase();
    if normalized == "auto" {
        return Err(ProviderError::Unsupported {
            requested: "auto".to_string(),
        });
    }
    match normalized.as_str() {
        "codex" | "openai" | "gpt-5" | "gpt-5.5" | "gpt-5.4" => Ok(SupportedProvider::Codex),
        "claude" | "anthropic" => Ok(SupportedProvider::Claude),
        "minimax" | "minimax-m2" | "minimax-m2.7" | "minimax-m3" | "abab" => {
            Ok(SupportedProvider::Minimax)
        }
        // Common Claude model aliases also map to Claude.
        "sonnet" | "opus" | "haiku" => Ok(SupportedProvider::Claude),
        other => Err(ProviderError::Unsupported {
            requested: other.to_string(),
        }),
    }
}

/// Like `parse_provider`, but if the caller passed `"auto"` or no provider
/// at all (empty string), pick the first provider that has configured auth
/// in `auth_registry`. Order is the canonical Phase 2 ordering
/// (codex → claude → minimax), so when multiple providers are configured
/// the same one is always chosen.
pub fn parse_provider_with_default(
    s: &str,
    auth_registry: &crate::auth::AuthRegistry,
) -> Result<SupportedProvider, ProviderError> {
    if s.trim().is_empty() || s.trim().eq_ignore_ascii_case("auto") {
        for candidate in [
            SupportedProvider::Codex,
            SupportedProvider::Claude,
            SupportedProvider::Minimax,
        ] {
            if auth_registry.has_auth(candidate) {
                return Ok(candidate);
            }
        }
        return Err(ProviderError::Unsupported {
            requested: "auto (no provider has configured auth)".to_string(),
        });
    }
    parse_provider(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_names() {
        assert_eq!(parse_provider("codex").unwrap(), SupportedProvider::Codex);
        assert_eq!(parse_provider("claude").unwrap(), SupportedProvider::Claude);
        assert_eq!(parse_provider("minimax").unwrap(), SupportedProvider::Minimax);
    }

    #[test]
    fn parses_provider_aliases() {
        assert_eq!(parse_provider("openai").unwrap(), SupportedProvider::Codex);
        assert_eq!(parse_provider("anthropic").unwrap(), SupportedProvider::Claude);
        assert_eq!(parse_provider("sonnet").unwrap(), SupportedProvider::Claude);
        assert_eq!(parse_provider("MiniMax-M3").unwrap(), SupportedProvider::Minimax);
    }

    #[test]
    fn rejects_unsupported_provider() {
        let err = parse_provider("gemini").unwrap_err();
        match err {
            ProviderError::Unsupported { requested } => {
                assert_eq!(requested, "gemini");
            }
        }
    }

    #[test]
    fn default_models_match_locked_phase2_plan() {
        assert_eq!(SupportedProvider::Codex.default_model(), "gpt-5.5");
        assert_eq!(SupportedProvider::Claude.default_model(), "claude-sonnet-4-6");
        assert_eq!(SupportedProvider::Minimax.default_model(), "MiniMax-M3");
    }
}
