# `mona`

`mona` is a Monitter-owned Rust harness, derived from [`jcode`](https://github.com/1jehuang/jcode) v0.86.0.

## Status

Phases 1 and 2 are complete on buildable, runnable binaries.

- The full `jcode` workspace (82 crates, ~80k lines) has been renamed to `mona`/`mona-*`.
- The legacy CLI surface is restricted to a single command: `mona acp`.
- All other commands from upstream are intentionally unreachable; they error with a hint to use `mona acp` or the dedicated `mona-acp` binary.
- The dedicated `mona-acp` binary owns the Monitter ACP lifecycle, per-turn
  routing, bounded tools, persistence, cancellation, and routing traces.

## What this binary does

`mona-acp` runs as an [Agent Client Protocol](https://github.com/zed-industries/agent-client-protocol) (ACP) stdio server. It accepts JSON-RPC 2.0 frames on stdin and emits ACP events on stdout. The legacy `mona acp` entry point remains the Phase 1 compatibility surface; Monitter should discover and launch the dedicated binary.

To integrate:

```sh
# Provider credentials remain host-owned and reloadable; mona-acp never
# refreshes OAuth or changes accounts implicitly.
# Place credentials at one of:
#   ~/.mona/codex.json               (OpenAI OAuth/API key)
#   ~/.mona/claude.json              (Anthropic OAuth/API key)
#   ~/.mona/minimax.json             (MiniMax API key)
#   …or set the standard env vars:
#   OPENAI_API_KEY=...
#   ANTHROPIC_API_KEY=...
#   MINIMAX_API_KEY=...

# Launch the ACP server on stdio (e.g. from Monitter desktop):
mona-acp
```

ACP clients (Monitter desktop, mobile, share-web) drive `mona-acp` via stdio JSON-RPC. No socket, no daemon, no port.

## Building

```sh
cargo build --release --bin mona --no-default-features
cargo build --release -p mona-acp --bin mona-acp
# Outputs: target/release/mona and target/release/mona-acp.
```

Note: AWS Bedrock support requires Rust 1.94.1+. Disable the `bedrock` feature (default-off via `--no-default-features`) if your toolchain is 1.94.0 or older. This is a transitive constraint of upstream's `aws-sdk-*` deps and is unchanged from `jcode`.

## Fork relationship with upstream

| Aspect | Source | Reason |
|---|---|---|
| All 80+ workspace crates | `github.com/1jehuang/jcode` (commit `e589cbe`, v0.86.0) | Verbatim fork of upstream |
| Crate / binary / path names | renamed `jcode` → `mona` | Branding for Monitter |
| `Provider` trait | upstream | Required by Phase 2 ACP server |
| ACP wire protocol | upstream-compatible (`agent-client-protocol = "0.10.4"`) | Stable across `jcode` and `mona` |
| Provider strings (`openai`, `claude`, etc.) | unchanged | Model family IDs, not harness names |
| `JcodeProvider`, `JcodeTier`, `RuntimeProviderId::Jcode` | unchanged | Refers to upstream's "Jcode Subscription" product; we don't ship that product but the type names remain |
| LICENSE | MIT verbatim from upstream | Required by license terms |
| Upstream re-sync | manual `git fetch upstream && git rebase` on a feature branch | Periodically pick up upstream fixes |

We **do not** maintain a fork relationship with upstream's issue tracker. For fork-specific issues, open them on `github.com/soyrex/mona`. For upstream issues, open them on `github.com/1jehuang/jcode`.

## Phase 2 architecture

Phase 2 runs turns through Mona's canonical `Agent` loop, so ACP sessions use
the same persisted transcript, provider continuation ID, compaction and
tool-result recovery behavior as the native harness. The Agent is constructed
with an empty registry and only four reviewed ACP adapters (`read`, `write`,
`bash`, and `ls`) plus host-supplied HTTP MCP tools. Mutating and remote tools
still cross Monitter's `allow_once`/`reject_once` boundary before execution;
embedding the Agent does not expose the upstream default tool registry.

The server keeps its bounded, redacted routing context and persists the
canonical conversation under `MONA_HOME/sessions`. It reconstructs provider
handles only from current non-expired credentials, supports concurrent
in-flight cancellation, and emits requested-versus-actual `router_trace`
updates. `off`, `recommend`, `safe_auto`, and `per_turn` routing policies never
change provider/account ownership or widen tool permissions.

Live Jev classification is network-off by default. Operators must explicitly
set `MONA_ACP_LIVE_JEV=1`; any other value keeps the deterministic rule-based
classifier. `MONA_ACP_JEV_PROVIDER` independently selects `auto`, `mona`,
`openrouter`, `typesafe`, or `aimlapi` from already configured credentials.
Mona does not copy credentials into session state or traces, and an unavailable
live configuration visibly falls back to the offline classifier. The
subscription route separately requires the `acp_jev` account capability.

## Next phases

- **Phase 3** — Cosmetic CLI strings (the help text still says `J-Code`, `Jcode daemon`, etc.) moved into the Monitter-owned `mona-acp` crate.
- **Phase 4** — Remove unused TUI/presentation crates from the workspace (`mona-tui*`, `mona-pdf`, `mona-render-core`, `mona-notify-email`, etc.). See `FUTURE-CLEANUPS.md`.

## License

`mona` is licensed under the MIT License. See `LICENSE` for the verbatim upstream license (Copyright (c) 2025 Jeremy Huang) and `MONA_NOTICE.md` for the attribution notice required by MIT.

Modifications by Holt Seafood Company Pty Ltd.
