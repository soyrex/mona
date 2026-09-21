# Milestone A: real provider construction

`session/new` now constructs upstream provider runtimes from the ACP auth registry.
The public `build_provider_for_session` signature remains unchanged. Sessions with
no configured auth still support listing/cancellation and report a null
`providerName`; their provider refuses completion rather than loading ambient auth.
The initial session model is passed to the real runtime explicitly.

| ACP provider | Runtime identity | Credentials |
| --- | --- | --- |
| codex | openai | OpenAI API key or unexpired Codex OAuth |
| claude | anthropic | Anthropic API key or unexpired Claude OAuth |
| minimax | minimax | Direct MiniMax API key, including coding-plan keys |

## Direct MiniMax

MiniMax uses its own API endpoint and bearer key. No OpenRouter account, API key,
routing, attribution headers, or model-catalog cache is used. The implementation
reuses upstream's OpenAI-compatible HTTP/SSE transport, which resides in the
historically named `mona-provider-openrouter-runtime` crate.

The existing auth loader reads `~/.mona/minimax.json` (or the `MONA_HOME` directory):

```json
{
  "kind": "minimax_api_key",
  "api_key": "YOUR_MINIMAX_CODING_PLAN_KEY",
  "api_base": "https://api.minimax.io/v1"
}
```

When the file is absent, `MINIMAX_API_KEY` supplies the key and the same default
base URL is used. An explicitly supplied `api_base` remains supported. MiniMax's
[plan documentation](https://platform.minimax.io/subscribe/coding-plan) describes
using plan API keys with OpenAI-compatible tools.

## Credential ownership

New embedding constructors preserve the caller's credentials through forks,
invalidation, and reload attempts. They do not replace them with the CLI's active
account or mutate process-wide auth environment variables. Anthropic embedding
construction also skips global usage initialization and official-client metadata
lookups. MiniMax construction avoids the named-profile environment mutation.
Existing CLI constructors retain their prior behavior.

Injected OAuth refresh fails with an explicit host-refresh error before calling
upstream's global credential persistence helpers. The embedding host must supply
fresh credentials and reconstruct the provider. There is no new ACP authentication
refresh endpoint in this milestone. Provider-specific non-auth settings in the
OpenAI runtime may still use upstream configuration.

## Verification

- `cargo check -p mona-acp` compiles the selected runtime dependencies. Optional
  Bedrock support is disabled for this crate.
- `cargo test -p mona-acp` exercises provider construction for all auth variants,
  explicit initial models, missing/mismatched credentials, and the actual stdio
  binary. Integration tests use Cargo's binary path instead of silently selecting
  an old release binary.
- `cargo test -p mona-provider-openai-runtime -p mona-provider-anthropic-runtime -p mona-provider-openrouter-runtime injected --lib`
  exercises credential isolation, refresh guards, and direct MiniMax transport.
- The MiniMax HTTP/SSE test uses a local listener and synthetic credentials. It
  checks the request path, model, bearer key, absent OpenRouter headers, and parsed
  text event. No live provider request or paid model turn was performed.

## Remaining work

`session/prompt` still runs the existing routing hook and returns its routing-only
response. Agent execution, streaming ACP updates, tool round-trips, cancellation
of running turns, durable resume, live model/effort updates, and host-managed OAuth
refresh remain separate work. Successful provider construction does not establish
that a credential is valid or that an authenticated model turn works.
