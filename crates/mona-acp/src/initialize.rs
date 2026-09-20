//! `initialize` route handler — capability negotiation.
//!
//! Returns the protocol version, the agent's capabilities, the list of
//! available auth methods, and the agent metadata. Capabilities include:
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

use serde_json::{Value, json};

/// ACP protocol version this server speaks. Matches upstream's
/// `ACP_PROTOCOL_VERSION = 1` constant in `src/cli/acp.rs:13`.
pub const ACP_PROTOCOL_VERSION: u64 = 1;

/// Build the JSON-RPC `result` payload for an `initialize` request.
pub fn initialize_result(server_name: &str, server_version: &str) -> Value {
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
                    "reasoning_effort": true
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

    #[test]
    fn result_advertises_phase2_capabilities() {
        let r = initialize_result("mona-acp", "0.1.0");
        assert_eq!(r["protocolVersion"], 1);
        assert_eq!(
            r["agentCapabilities"]["extensions"]["monitter"]["jev_routing"],
            true
        );
        assert_eq!(
            r["agentCapabilities"]["extensions"]["monitter"]["reasoning_effort"],
            true
        );
        assert_eq!(r["agentInfo"]["monitter_harness"], true);
    }

    #[test]
    fn result_lists_three_supported_providers() {
        let r = initialize_result("mona-acp", "0.1.0");
        let methods = r["authMethods"].as_array().unwrap();
        let ids: Vec<&str> = methods
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["codex", "claude", "minimax"]);
    }
}
