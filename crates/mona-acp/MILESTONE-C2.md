# Milestone C.2: live Jev route application

`session/prompt` now applies a safety-approved Jev route to the session's real
provider runtime before starting the model turn. The router keeps provider and
credential ownership fixed: a Codex session can only select Codex models, a
Claude session can only select Claude models, and a MiniMax session can only
select MiniMax models.

The initial provider-scoped tier map is deliberately small and static:

| Tier | Codex | Claude | MiniMax |
| --- | --- | --- | --- |
| Fast | `gpt-5.1-codex-mini` | `claude-haiku-4-5` | `MiniMax-M2.7-highspeed` |
| Balanced | `gpt-5.4` | `claude-sonnet-4-6` | `MiniMax-M2.7` |
| Strong | `gpt-5.5` | `claude-opus-4-8` | `MiniMax-M3` |
| Frontier | `gpt-6-astra` | `claude-opus-5` | `MiniMax-M3` |

## Atomic application and rollback

Model and effort changes are prepared against `Provider::fork()`. The current
session handle is untouched while the candidate is validated and configured.
Only a fully successful candidate replaces the live handle and its persisted
session metadata. If either setting is rejected, the session continues with
the previous runtime and the trace records the failure.

The same path now backs `session/set_model` and
`session/set_reasoning_effort`; these methods no longer acknowledge metadata-
only changes. They require an authenticated session and return the runtime's
actual model and effort. Provider normalization is therefore reported rather
than overwritten with the requested spelling.

## Truthful traces and ACP output

Router traces distinguish the proposal from the live result:

- `requested_model` and `requested_effort` preserve the concrete Jev request.
- `new_model` and `new_effort` contain the actual post-attempt runtime state.
- `applied` is true only after the candidate is installed.
- `application_error` explains a failed live application.

ACP emits the same distinction in a `router_trace` session update. A successful
`session/prompt` response also includes the finalized routing object alongside
the actual model and effort used for the turn.

## Safety boundary

Jev cannot change the session provider, credentials, permission tier, tool
approval behavior, or authentication state. Sensitive prompts still stop at
the existing human-review gate. Unauthenticated sessions retain lifecycle and
trace behavior but cannot claim a live route was applied. The per-session swap
budget is enforced before runtime mutation.

## Verification

- `cargo test -p mona-acp --lib`
- `cargo test -p mona-acp --test end_to_end`
- `cargo check -p mona-acp`
- injected provider-runtime regression tests for OpenAI, Anthropic, and
  OpenRouter
- fixture-backed live Decisions adapter tests with no credentials or network

Coverage includes provider-scoped tier resolution, successful model and effort
application, provider normalization, atomic rollback, unauthenticated failure,
safety refusal, swap-budget refusal, requested-versus-actual trace fields,
standard and legacy ACP prompt shapes, and manual setter persistence.

No paid provider turn is part of this verification. Authenticated tests use
synthetic credentials only to construct and reconfigure local provider
runtimes; they do not make network requests.

## Live Jev activation

The default remains the deterministic, offline `RuleBasedClassifier`. Live
classification requires the exact process opt-in `MONA_ACP_LIVE_JEV=1` and a
separately resolved `MONA_ACP_JEV_PROVIDER` route. Invalid activation values or
unavailable configuration visibly fall back to the offline classifier without
performing a request. The subscription path requires the distinct `acp_jev`
capability; it does not reuse memory or browser entitlement.

## Deliberate follow-ons

- Make the provider tier map operator-configurable while keeping the same
  provider/auth boundary.
- Run an explicitly approved paid provider smoke through Monitter when one is
  available.
