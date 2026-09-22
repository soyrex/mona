# Live Jev routing

Mona ACP has separate switches for classifier selection and route application.
To use the existing TypeSafe account persist this non-secret configuration in
`$MONA_HOME/mona-acp.json` (default `~/.mona/mona-acp.json`):

```json
{
  "liveJev": true,
  "jevProvider": "typesafe",
  "jevRoutePolicy": "safe_auto"
}
```

Keep `TYPESAFE_API_KEY` in Monitter's Keychain-backed Environment & Secrets.
Monitter injects it into newly launched local user ACP agents. Do not put it in
this file, agent arguments, transcripts, or source control. Existing resident
ACP processes need a new launch to load configuration or a replacement binary.

`MONA_ACP_LIVE_JEV=1` is an explicit environment override; the environment-only
path supports `MONA_ACP_JEV_PROVIDER`. Invalid activation or unavailable explicit
live configuration never silently selects the keyword classifier. A transient
classification failure can retain the current model or reuse a previous plan,
but the router trace identifies that fallback as classifier unavailable, not a
fresh live decision. Routing policy `off` skips classification entirely.

## Model matrix

`src/model_profiles.rs` is a versioned static seed. It separates sourced vendor
descriptions from operator routing preferences and includes source URLs and
review dates. The classifier receives only profiles intersecting the session's
authenticated model catalogue; the matrix never grants access or changes the
session provider. Unknown IDs remain unknown, and live provider capabilities
remain authoritative for supported efforts and availability.

The initial research uses official OpenAI model pages for Astra, Sol, Terra and
Luna and MiniMax's official token-plan page for identifier confirmation. It does
not invent prices, benchmark scores, account entitlements, or capabilities for
undocumented entries. Revisit source descriptions when updating the model
catalogue; this is not a live price feed or a measured routing benchmark.

Seed reviewed 2026-09-22:

| Model | Sourced vendor positioning | Operator routing preference |
| --- | --- | --- |
| [GPT-5.6 Luna](https://developers.openai.com/api/docs/models/gpt-5.6-luna.md) | Cost-sensitive, high-volume work | Extraction, lookup, narrow mechanical edits |
| [GPT-5.6 Terra](https://developers.openai.com/api/docs/models/gpt-5.6-terra.md) | Intelligence/cost balance | Ordinary bug fixes, contained features |
| [GPT-5.6 Sol](https://developers.openai.com/api/docs/models/gpt-5.6-sol.md) | Complex professional work | Difficult debugging, multi-file refactoring, careful review |
| [GPT-6 Astra](https://developers.openai.com/api/docs/models/gpt-6-astra.md) | Hardest end-to-end work | Architecture, ambiguous cross-system reasoning |
| [MiniMax M2.7 / highspeed / M3](https://platform.minimax.io/subscribe/token-plan?tab=api-enterprise) | Identifier confirmation only; capability details unknown in this seed | Existing tier policy, not a measured vendor ranking |

Other authenticated models are retained as unknown profiles. A short follow-up
does not imply a simpler task. Runtime availability/effort settings override
static descriptions, and no profile grants a tool or permission.

## Verification

Normal tests do not call a provider. An explicit live classification smoke is:

```sh
cargo test -p mona-acp --test live_jev_smoke -- --ignored --nocapture
```

Run it only with an authorized existing account and the intended provider
selected. It makes a real request using a synthetic greeting and reports only
the resulting tier, effort and confidence. A successful classification is not
proof of an authenticated agent turn; verify the installed binary through ACP
separately and inspect its persisted router trace.
