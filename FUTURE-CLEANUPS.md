# Future cleanups — Phase 1 surface strip is incomplete

`mona acp` is the only reachable user-facing command, but the underlying binary still links the entire upstream workspace (80+ crates, ~60 MB Mach-O arm64 binary, ~80k lines of code). This file enumerates what's still on disk but no longer reachable from the CLI, so future cleanup passes can prune it deliberately instead of guessing.

## Scope of the current strip

We modified exactly two files:

- `src/cli/dispatch.rs` — replaced the giant match block (lines 137–633 of the original) with one that only handles `Command::Acp`. All other variants now error with a hint to use `mona acp`. Net: ~450 lines removed.
- `src/cli/args.rs` — replaced the `Command::Acp` doc comment from "backed by the Jcode daemon" to "backed by the Monitter harness" and updated the top-level `--about` text. The `Command` enum still exists in full because clap needs it for `--help` parsing, even though most arms now error at dispatch time.

We did **not** touch `Cargo.toml` workspace members, `src/cli/args.rs` subcommand definitions, `src/lib.rs` re-exports, `src/main.rs` runtime setup, or any `crates/*/src/lib.rs`. Everything is still linked.

## Crates that can be deleted (workspace members)

The 24 crates below are TUI/display/presentation-only. They are listed in `Cargo.toml` `[workspace.members]` and have no inbound references from `src/cli/acp.rs` or from each other that aren't TUI-related.

| Crate | Why it's dead |
|---|---|
| `crates/mona-tui` | The umbrella TUI crate. `src/lib.rs:22` does `pub use mona_tui::*`; can become `pub use mona_app_core::*; pub use mona_base::*;` after this is deleted. |
| `crates/mona-tui-markdown` | Markdown rendering for the TUI. |
| `crates/mona-tui-messages` | Chat-bubble rendering. |
| `crates/mona-tui-core` | Shared TUI types. |
| `crates/mona-tui-mermaid` | Mermaid diagram rendering. |
| `crates/mona-tui-account-picker` | Account picker widget. |
| `crates/mona-tui-anim` | Animation utilities. |
| `crates/mona-tui-render` | Render layer. |
| `crates/mona-tui-visual-debug` | Debug overlays. |
| `crates/mona-tui-session-picker` | Session picker widget. |
| `crates/mona-tui-style` | Theme/styles. |
| `crates/mona-tui-permissions` | Permission-request widget. |
| `crates/mona-tui-tool-display` | Tool-result display. |
| `crates/mona-tui-usage-overlay` | Usage ring overlay. |
| `crates/mona-tui-workspace` | Multi-pane workspace. |
| `crates/mona-render-core` | Render helpers (mermaid/html). |
| `crates/mona-pdf` | PDF rendering. Used only by `tui-render`. |
| `crates/mona-notify-email` | Email notification subsystem. |
| `crates/mona-terminal-image` | Terminal graphics (Sixel/iTerm). |
| `crates/mona-terminal-launch` | Terminal handoff to a child process. ACP uses `snapshot_client_terminal_env` (line 138) which can be inlined into `acp.rs` (5-line function). |
| `crates/mona-embedding` | 87 MB ONNX embedding model. Loaded by ambient-mode and `mona-memory-types`. Not reachable from `mona acp`. |
| `crates/mona-update-core` | Self-update logic. |
| `crates/mona-productivity-core` | Productivity tools (e.g., todo, calendar). |
| `crates/mona-overnight-core` | Long-running task scheduling. |

**Estimated binary size saving after deleting all of these**: ~30 MB (60 MB → ~30 MB).
**Estimated compile time saving**: ~5 min on cold builds; ~80% faster on incremental builds.

## Crates that can be deleted but are referenced by `acp.rs` (require inlining first)

| Crate | Reference in `src/cli/acp.rs` | Action |
|---|---|---|
| `crates/mona-terminal-launch` | `crate::terminal_launch::snapshot_client_terminal_env()` at lines 778, 832 | Inline a 5-line function: capture `std::env::vars()` filtered for terminal-related keys. |
| `src/cli/provider_init.rs` (not a crate, but ~1900 lines) | `super::provider_init::ProviderChoice` at line 2 | ProviderChoice is only used as a clap arg type. Replace with a minimal enum that maps to the same `provider` env var upstream expects. Save ~1800 lines. |
| `src/cli/commands/` (directory, not a crate) | None reachable from `mona acp` | All commands in this directory are CLI-only (menubar, restart save/restore, etc.). Delete the directory; remove `pub mod commands;` from `src/cli/mod.rs`. ~3000 lines. |
| `src/cli/tui_launch/` (directory) | None reachable from `mona acp` | TUI launchers and replays. ~1000 lines. |
| `src/cli/terminal.rs` | None reachable | ~880 lines of terminal-multiplexer plumbing. |
| `src/cli/macos_notification_broker.rs` | Referenced from `src/main.rs:131–133` | Remove the call sites in `main.rs` and delete the file. |
| `src/cli/selfdev*.rs` | None reachable | Self-development mode. Path-gated, but unused. ~700 lines. |
| `src/cli/auth_test/*` | None reachable | Provider credential validator. Useful diagnostic, but not on the ACP path. ~1800 lines. |
| `src/cli/provider_doctor.rs` | None reachable | Provider health check. ~600 lines. |
| `src/cli/auth_import.rs` | None reachable | Bulk-import auth from CSV. |
| `src/cli/output.rs` | None reachable | Terminal output formatting. |
| `src/cli/account.rs` | None reachable | Jcode Cloud account management. |
| `src/cli/debug.rs` | None reachable | Debug socket client. |
| `src/cli/hot_exec.rs` | None reachable | Process replacement helper. |
| `src/cli/proctitle.rs` | None reachable | Process title setting. |
| `src/cli/ssh*.rs` | None reachable | SSH transport for remote `jcode serve`. ~1100 lines. |
| `src/cli/telemetry.rs` | Reachable via `Command::Telemetry` (now errors) | Diagnostic command. Can keep if you want `MONA_NO_TELEMETRY` support; delete if you don't. |
| `src/cli/login.rs` + `src/cli/login/*` | Reachable via `Command::Login` (now errors) | Provider OAuth/login. ~3200 lines. The auth loaders in `mona-base/src/auth/` should stay. |
| `src/cli/args.rs` | All subcommand definitions (now errors) | Once the deleted arms are also removed from the `Command` enum, `args.rs` shrinks from 1124 lines to ~100. |
| `src/cli/dispatch.rs` | Most of it | Once the dead subcommand modules are deleted, `dispatch.rs` shrinks from 1052 lines to ~250. |

## Crates that must stay (referenced from `acp.rs`)

| Crate | Reference in `acp.rs` |
|---|---|
| `crates/mona-app-core` | `crate::provider`, `crate::protocol`, `crate::transport`, `crate::compaction`, `crate::server`, `crate::env`, `crate::config`, `crate::agent` (re-exported via `mona-tui`) |
| `crates/mona-base` | `crate::auth`, `crate::external_auth`, `crate::provider` (re-exported) |
| `crates/mona-provider-core` | `crate::provider` (re-exported) |
| `crates/mona-provider-openai` | Auto-detected by `mona-base/src/auth/codex.rs` |
| `crates/mona-provider-anthropic` | Same |
| `crates/mona-provider-openrouter` | Same |
| `crates/mona-message-types` | `crate::protocol::HistoryMessage` |
| `crates/mona-transport` | `crate::transport::{ReadHalf, WriteHalf}` |
| `crates/mona-protocol` | `crate::protocol::{Request, ServerEvent}` |
| `crates/mona-harness-api-server` | Will become Phase 2's ACP server (currently `acp.rs` reimplements the JSON-RPC loop; Phase 2 should reuse `harness-api-server`'s framing) |

## `src/main.rs` — what can be removed

- `cli_launch_hint_source_invocation()` and `is_macos_hotkey_listener_invocation()` (lines 142–164): only relevant for `mona setup-hotkey`, which is unreachable.
- The `cli_launch_hint_source_invocation()` early return at line 114–116.
- The `is_macos_hotkey_listener_invocation()` early return at line 123–125.
- The `mona::cli::macos_notification_broker::is_invocation()` block at line 131–133.
- The Windows-specific `WINDOWS_MAIN_STACK_SIZE` logic (line 81–98) is harmless but unused once the broker is gone.
- The `parse_alloc_tuning_env` and `mallopt` config (line 30–56): Linux-only glibc tuning. Useful but not load-bearing for ACP. Keep for now (helps RSS).
- The jemalloc features (line 1–26): keep; they're harmless and may help.

Net: `src/main.rs` shrinks from 236 lines to ~70 lines after this pass.

## `src/lib.rs` — what can be removed

Currently:

```rust
pub use mona_tui::*;
pub mod cli;
```

Replace with:

```rust
pub use mona_app_core::*;
pub use mona_base::*;
// explicit re-exports for the few items acp.rs needs that aren't in those two
pub use mona_message_types::{...};
pub use mona_transport::{ReadHalf, WriteHalf};
pub use mona_protocol::{Request, ServerEvent};
pub mod cli;
```

Or, simpler: keep `pub use mona_tui::*` and just delete the TUI crates in dependency order. `mona_tui` will fail to compile, but its current re-export pattern (`pub use jcode_app_core::*; pub use jcode_base::*;`) means the binary only loses what was TUI-specific.

## Cosmetic strings that should change

These are user-facing but the binary currently says upstream-flavored copy:

| String | Where | Current | Target |
|---|---|---|---|
| `--about` (top-level help) | `src/cli/args.rs:32` | "J-Code: A coding agent using Claude Max or ChatGPT Pro subscriptions" | "mona: Monitter-owned ACP harness (forked from jcode). Run `mona acp` to start an Agent Client Protocol stdio server." (✅ done in Phase 1) |
| `Command::Acp` doc comment | `src/cli/args.rs:170` | "Run as an Agent Client Protocol (ACP) adapter backed by the Jcode daemon" | "Run as an Agent Client Protocol (ACP) stdio server. Monitter's per-turn Jev routing happens here." |
| `--provider <PROVIDER>` help | `src/cli/args.rs:34` | Lists 50+ providers as a single line | Trim to the ones Monitter actually supports (OpenAI, Anthropic, OpenRouter) and add "(others are available but not part of the Monitter-tested matrix)" |
| `Self::Jcode` runtime provider | `src/cli/provider_init.rs:211` | The "Jcode Subscription" provider | Either hide it from `--provider` autocomplete, or rename it to `Self::Mona` (would require coordinated changes in `mona-base/src/provider/activation.rs`) |

## Effort estimate for the full cleanup

- Delete TUI crates: 30 min work + 5 min compile.
- Inline `terminal_launch::snapshot_client_terminal_env`: 30 min.
- Inline/replace `provider_init::ProviderChoice`: 1–2 hours (touches many files).
- Delete CLI directories (`commands/`, `tui_launch/`, `terminal.rs`, etc.): 1 hour.
- Trim `args.rs` and `dispatch.rs` to minimal: 30 min.
- Trim `main.rs`: 15 min.
- Verify build still works at each step: 5 min × 5 steps = 25 min.

**Total: 4–6 hours of work, ~30 min of compile time.** After this, the binary is ~30 MB instead of ~60 MB.

## Risks

- `acp.rs` calls `crate::terminal_launch::snapshot_client_terminal_env()` which captures the user's `TERM`, `COLORTERM`, etc. when launched by SSH clients. Inlining a 5-line `std::env::vars().filter(...).collect()` works but loses the precise key list. The exact list is in `crates/mona-terminal-launch/src/lib.rs:138–170`; copy it verbatim.
- The `ProviderChoice` enum has 50+ variants and aliases. Replacing it with a minimal version means the `mona acp --provider` flag accepts fewer values; users on providers outside the short list need an env-var override path.
- `JcodeSubscription` is a real upstream product with a real API key. Renaming its enum variant would require coordinated changes across ~10 files and would diverge from upstream's identity in a way that complicates re-syncs. **Recommendation: leave the type names alone**; they're internal and don't appear in user-facing strings after the cosmetic pass.

## Verification checklist for the cleanup pass

After each deletion, confirm:

```sh
cargo check --workspace --no-default-features  # should still pass with 0 errors
cargo build --release --bin mona --no-default-features  # should still produce a binary
./target/release/mona acp --help  # should print the ACP help
./target/release/mona serve  # should error: "`Serve` is not supported..."
./target/release/mona  # should print the banner
./target/release/mona version  # should report "v0.86.0-dev (dirty)"
```
