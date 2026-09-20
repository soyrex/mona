//! `mona-acp` — Monitter-owned ACP stdio server with per-turn Jev routing.
//!
//! Phase 2 of the mona fork. This crate exposes a single binary,
//! `mona-acp`, that speaks the upstream-compatible ACP JSON-RPC protocol
//! over stdio. The protocol wire shape matches what `crates/jcode/src/cli/acp.rs`
//! in upstream already speaks, so any existing Monitter ACP client can
//! drive `mona-acp` without code changes.
//!
//! Phase 2 status: server loop, capability negotiation, provider whitelist,
//! and session lifecycle are real. `session/prompt` returns a stub response
//! acknowledging the prompt but does not yet drive a real agent loop.
//! The full `Agent::run_turn_with_jev` integration lands in Phase 2.5.
//!
//! See `docs/PHASE-2-MONA-ACP.md` for the full plan and Phase 2.5 scope.

#![forbid(unsafe_code)]

pub mod auth;
pub mod initialize;
pub mod policy;
pub mod provider_whitelist;
pub mod server;
pub mod session;
pub mod trace;
pub mod turn;

pub use auth::{Auth, AuthRegistry};
pub use initialize::initialize_result;
pub use policy::JevRoutePolicy;
pub use provider_whitelist::{SupportedProvider, parse_provider};
pub use server::{run_acp_server, ServerState};
pub use trace::{RouterTrace, TraceTrigger};
pub use turn::{RouterConfig, TurnRoutingDecision, run_turn_with_jev};
