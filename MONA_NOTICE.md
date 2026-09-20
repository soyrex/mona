# `mona` license notice

`mona` is a derivative of [`jcode`](https://github.com/1jehuang/jcode) v0.86.0, licensed under the MIT License.

## Upstream copyright

```
MIT License

Copyright (c) 2025 Jeremy Huang
```

The full MIT license text is preserved verbatim in [`LICENSE`](./LICENSE) at the root of this repository, as required by the MIT terms.

## Modifications

Modifications and the `mona` rebrand are by **Holt Seafood Company Pty Ltd** (https://github.com/soyrex/mona).

## What's preserved from upstream

- All source files retain their MIT license headers where they exist.
- The `LICENSE` file is verbatim from upstream; nothing has been altered.
- Provider strings, wire protocol types, and the `JcodeProvider`/`RuntimeProviderId::Jcode` types are intentionally left unchanged because they refer to upstream's "Jcode Subscription" product (a third-party service we do not ship). Renaming them would require renaming all references to a third-party API surface, which is out of scope for a license-preserving fork.

## What's new in `mona`

- Workspace member names: `jcode-*` → `mona-*` (82 crates).
- Binary name: `jcode` → `mona`.
- Default home directory: `~/.jcode/` → `~/.mona/`.
- Default socket paths: `~/.jcode/jcode.sock` → `~/.mona/mona.sock`; `~/.jcode/jcode-api.sock` → `~/.mona/api.sock`.
- Default env var prefix: `JCODE_*` → `MONA_*`.
- Telemetry env: `JCODE_NO_TELEMETRY` → `MONA_NO_TELEMETRY`.
- CLI surface: restricted to a single command `mona acp`. All other upstream commands are intentionally unreachable.
- `MONA_NOTICE.md` (this file).

## Re-sync with upstream

If you re-sync with `1jehuang/jcode:master`, do **not** overwrite `LICENSE` or `MONA_NOTICE.md`. They are intentionally divergent from upstream and the divergence is what makes the fork legally distributable.

If upstream re-licenses under anything other than MIT, the fork must either (a) follow the new license or (b) fork from the last MIT-licensed commit. Until then, MIT terms apply.
