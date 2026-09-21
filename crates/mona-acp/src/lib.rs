//! `mona-acp` — Monitter-owned ACP stdio server with per-turn Jev routing.
//!
//! Phase 2 of the mona fork. This crate exposes a single binary,
//! `mona-acp`, that speaks the upstream-compatible ACP JSON-RPC protocol
//! over stdio. The protocol wire shape matches what `crates/jcode/src/cli/acp.rs`
//! in upstream already speaks, so any existing Monitter ACP client can
//! drive `mona-acp` without code changes.
//!
//! Current status: provider construction and streamed model turns are real.
//! Milestone C.1 adds a bounded in-server tool loop with ACP one-time
//! permissions for mutating tools. Milestone C.2 applies Jev-selected model
//! and effort changes atomically to the live provider and reports requested
//! versus actual routing outcomes. Durable history and mid-turn cancellation
//! remain later milestones.

#![forbid(unsafe_code)]

pub mod auth;
pub mod initialize;
pub mod policy;
pub mod provider;
pub mod provider_whitelist;
pub mod server;
pub mod session;
pub mod trace;
pub mod turn;

pub use auth::{Auth, AuthRegistry};
pub use initialize::initialize_result;
pub use policy::JevRoutePolicy;
pub use provider::{ProviderHandle, build_provider_for_session};
pub use provider_whitelist::{SupportedProvider, parse_provider};
pub use server::{run_acp_server, ServerState};
pub use trace::{RouterTrace, TraceTrigger};
pub use turn::{RouterConfig, TurnRoutingDecision, run_turn_with_jev};
