# `mona`

`mona` is a Monitter-owned Rust harness, derived from [`jcode`](https://github.com/1jehuang/jcode) v0.86.0.

## Status

Phase 1 shipped on a buildable, runnable binary.

- The full `jcode` workspace (82 crates, ~80k lines) has been renamed to `mona`/`mona-*`.
- The CLI surface is restricted to a single command: `mona acp`.
- All other commands from upstream are intentionally unreachable; they error with a hint to use `mona acp`.

## What this binary does

`mona acp` runs as an [Agent Client Protocol](https://github.com/zed-industries/agent-client-protocol) (ACP) stdio server. It accepts JSON-RPC 2.0 frames on stdin and emits ACP events on stdout. The protocol surface is the same one `jcode` already shipped via `jcode acp`; we have not changed it in Phase 1.

To integrate:

```sh
# One-time provider setup (until Phase 2 adds a dedicated login flow).
# Place credentials at one of:
#   ~/.mona/openai-auth.json         (OpenAI OAuth)
#   ~/.mona/anthropic-auth.json      (Anthropic OAuth/API key)
#   ~/.mona/openrouter.json          (OpenRouter API key)
#   …or set the standard env vars:
#   OPENAI_API_KEY=...
#   ANTHROPIC_API_KEY=...

# Launch the ACP server on stdio (e.g. from Monitter desktop):
mona acp
```

ACP clients (Monitter desktop, mobile, share-web) drive `mona acp` via stdio JSON-RPC. No socket, no daemon, no port.

## Building

```sh
cargo build --release --bin mona --no-default-features
# Output: target/release/mona (~59 MB Mach-O arm64, includes all upstream
# provider + tool + agent-runtime crates; Phase 1 strip is surface-only).
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

## Next phases

This is Phase 1. The remaining phases (each its own milestone, each its own commit):

- **Phase 2** — A dedicated `mona-acp` crate that owns the ACP server loop with the per-turn Jev routing hook embedded in `Agent::run_turn`. The current `mona acp` is upstream's ACP implementation with a stripped CLI; Phase 2 builds a Monitter-native replacement.
- **Phase 3** — Cosmetic CLI strings (the help text still says `J-Code`, `Jcode daemon`, etc.) moved into the Monitter-owned `mona-acp` crate.
- **Phase 4** — Remove unused TUI/presentation crates from the workspace (`mona-tui*`, `mona-pdf`, `mona-render-core`, `mona-notify-email`, etc.). See `FUTURE-CLEANUPS.md`.

## License

`mona` is licensed under the MIT License. See `LICENSE` for the verbatim upstream license (Copyright (c) 2025 Jeremy Huang) and `MONA_NOTICE.md` for the attribution notice required by MIT.

Modifications by Holt Seafood Company Pty Ltd.
