# Milestone C.1: bounded ACP tool execution

`session/prompt` now advertises four in-server tools and continues a provider
turn after executing them:

- `read_file` and `ls` are read-only and confined to the session working tree.
- `write_file` and `bash` require an ACP `session/request_permission` request.
- Permission requests offer only `allow_once` and `reject_once`; missing,
  malformed, expired, or disconnected responses fail closed.
- Tool calls and matching results are replayed to the provider as an assistant
  `ToolUse` followed by a user `ToolResult`.

The original C.1 implementation used a small eight-round ACP loop. Phase 2
closeout replaced that loop with Mona's canonical `Agent` while retaining the
same restricted ACP tool adapters and one-time permission boundary. ACP turns
now share the native harness's complete transcript replay, provider
continuation, compaction, recovery, persistence, and cancellation behavior.

## Session HTTP MCP

ACP hosts may provide `mcpServers` to `session/new` or `session/resume`.
`mona-acp` accepts only Streamable HTTP (`http` or `streamable-http`) endpoints;
stdio and legacy SSE are rejected so a host cannot cause a local command spawn.
The endpoint headers, negotiated MCP session ID, and discovered tools stay
runtime-only for that ACP session. Remote tools are namespaced as
`mcp__<server>__<tool>`, registered into the same restricted canonical Agent,
and require one-time ACP permission before every remote call.

HTTP setup is bounded to 20 seconds. The client uses MCP protocol `2025-03-26`
and sends `MCP-Protocol-Version` plus the host-supplied headers on each request.

The stdio transport uses one ordered request worker while the stdin reader stays
live. This preserves `initialize` -> `session/new` -> `session/prompt` ordering
and lets Monitter return permission responses while a prompt is awaiting them.
Tool implementations are asynchronous, so shell execution does not block the
ACP reader.

## Safety boundary

`bash` runs `/bin/sh -c` in the requested working directory and inherits the
harness environment. The working directory is not a sandbox. It therefore
always requires explicit one-time host approval. `write_file` also requires
approval and rejects absolute paths, parent traversal, and symlink escapes.
Read-only path tools enforce the same working-tree boundary.

There is no production auto-approval implementation. The only `AlwaysAllow`
implementation is compiled for deterministic unit tests.

## Verification

- `cargo test -p mona-acp-tools -p mona-acp --lib`
- `cargo test -p mona-acp --test end_to_end`
- `cargo check -p mona-acp`

Coverage includes traversal and write-through-symlink rejection, shell
output/exit status, real ACP one-time option IDs, rejection without side
effects, provider-visible `ToolUse`/`ToolResult` history, cross-turn transcript
replay, canonical-session resume, notification output, the existing stdio
lifecycle tests, and the three provider-construction paths.

No paid provider turn is part of this verification. The existing opt-in MiniMax
smoke remains the live integration check.

## Deliberate follow-ons

- Jev-selected model and effort application is complete in Milestone C.2.
- Durable canonical context/resume and in-flight provider cancellation are
  complete in the Phase 2 closeout.
- Add a live Monitter approval smoke with an authenticated provider when an
  explicit paid-turn approval is available.
