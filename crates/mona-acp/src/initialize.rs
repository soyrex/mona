//! `initialize` route handler — capability negotiation.
//!
//! Returns the protocol version, the agent's capabilities, the list of
//! available auth methods, the agent metadata, and (Phase 3) the list of
//! providers that have configured credentials.
//!
//! Capabilities include:
//!
//! - `session_model`: true (we support `session/set_model`).
//! - `session_resume`: true (we support `session/resume`).
//! - `session_usage`: true (we expose per-turn token counts).
//! - `jev_routing`: true (Phase 2 default; the per-turn Jev hook is wired).
//! - `reasoning_effort`: true (Monitter extension; `session/set_reasoning_effort`).
//!
//! The wire shape mirrors what `crates/jcode/src/cli/acp.rs:1631` in
//! upstream already emits, so existing Monitter ACP clients can drive
//! `mona-acp` without code changes.

use crate::auth::AuthRegistry;
use serde_json::{Value, json};

/// ACP protocol version this server speaks. Matches upstream's
/// `ACP_PROTOCOL_VERSION = 1` constant in `src/cli/acp.rs:13`.
pub const ACP_PROTOCOL_VERSION: u64 = 1;

/// Build the JSON-RPC `result` payload for an `initialize` request.
pub fn initialize_result(server_name: &str, server_version: &str, auth: &AuthRegistry) -> Value {
    let configured: Vec<String> = auth
        .configured_providers()
        .into_iter()
        .map(|p| p.as_str().to_string())
        .collect();
    json!({
        "protocolVersion": ACP_PROTOCOL_VERSION,
        "agentCapabilities": {
            "sessionCapabilities": {
                "model": {},
                "resume": {},
                "usage": {}
            },
            "extensions": {
                "monitter": {
                    "jev_routing": true,
                    "reasoning_effort": true,
                    "auth_loader": true
                }
            }
        },
        "authMethods": [
            {
                "id": "codex",
                "name": "OpenAI / Codex (OAuth)",
                "description": "Codex CLI OAuth session, used as the 'codex' provider."
            },
            {
                "id": "claude",
                "name": "Anthropic Claude (OAuth)",
                "description": "Claude subscription OAuth session, used as the 'claude' provider."
            },
            {
                "id": "minimax",
                "name": "MiniMax (API key)",
                "description": "MiniMax provider API key, used as the 'minimax' provider."
            }
        ],
        "configuredProviders": configured,
        "agentInfo": {
            "name": server_name,
            "version": server_version,
            "monitter_harness": true
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthRegistry;
    use crate::provider_whitelist::SupportedProvider;

    #[test]
    fn result_advertises_phase3_capabilities() {
        let registry = AuthRegistry::default();
        let r = initialize_result("mona-acp", "0.1.0", &registry);
        assert_eq!(r["protocolVersion"], 1);
        assert_eq!(
            r["agentCapabilities"]["extensions"]["monitter"]["jev_routing"],
            true
        );
        assert_eq!(
            r["agentCapabilities"]["extensions"]["monitter"]["reasoning_effort"],
            true
        );
        assert_eq!(
            r["agentCapabilities"]["extensions"]["monitter"]["auth_loader"],
            true
        );
        assert_eq!(r["agentInfo"]["monitter_harness"], true);
    }

    #[test]
    fn result_lists_three_supported_providers() {
        let registry = AuthRegistry::default();
        let r = initialize_result("mona-acp", "0.1.0", &registry);
        let methods = r["authMethods"].as_array().unwrap();
        let ids: Vec<&str> = methods
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["codex", "claude", "minimax"]);
    }

    #[test]
    fn result_reports_configured_providers() {
        let mut registry = AuthRegistry::default();
        registry.inner_mut().insert(
            SupportedProvider::Codex,
            crate::auth::Auth::OpenaiApiKey { api_key: "sk-test".into() },
        );
        let r = initialize_result("mona-acp", "0.1.0", &registry);
        let configured: Vec<String> = r["configuredProviders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(configured, vec!["codex"]);
    }
}

