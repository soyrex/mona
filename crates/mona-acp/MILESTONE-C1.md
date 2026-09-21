# Milestone C.1: bounded ACP tool execution

`session/prompt` now advertises four in-server tools and continues a provider
turn after executing them:

- `read_file` and `ls` are read-only and confined to the session working tree.
- `write_file` and `bash` require an ACP `session/request_permission` request.
- Permission requests offer only `allow_once` and `reject_once`; missing,
  malformed, expired, or disconnected responses fail closed.
- Tool calls and matching results are replayed to the provider as an assistant
  `ToolUse` followed by a user `ToolResult`.
- A turn is capped at eight provider/tool rounds.

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
effects, provider-visible parallel `ToolUse`/`ToolResult` history,
notification output, the eight existing stdio lifecycle tests, and the three
provider-construction paths.

No paid provider turn is part of this verification. The existing opt-in MiniMax
smoke remains the live integration check.

## Deliberate follow-ons

- Apply Jev-selected model and effort to the live provider rather than session
  metadata only.
- Persist multi-turn conversation history and durable resume state.
- Interrupt a running provider or shell process from `session/cancel`.
- Add a live Monitter approval smoke with an authenticated provider when an
  explicit paid-turn approval is available.
