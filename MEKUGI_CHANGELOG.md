# Mekugi native fork changelog

This is the current inventory of intentional differences from stock Codex.
It covers the native runtime Mekugi ships, not the host UI or changes already
in upstream Codex.

The fork started at `rust-v0.169.0`. The current comparison baseline is
`rust-v0.171.0` (imported in `221fbc5eed`). Upstream imports are squashes.
Compare the release tree directly with the fork tree, not their Git merge base:

```sh
git diff rust-v0.171.0 HEAD
git log --first-parent --oneline rust-v0.169.0..HEAD
```

## Runtime and distribution

- The app-server binary is named `mekugi-app-server`. The shipped runtime also
  includes `codex-code-mode-host` and, on Linux, `bwrap`. Upstream clients and
  SDKs remain in the source tree but are not shipped or maintained as part of
  Mekugi. The [Cargo workspace](codex-rs/Cargo.toml) and lockfile cover the
  native build; the fork uses Mekugi's release process, not upstream release CI.
- Subprocess regression tests are in a separate
  [crate](codex-rs/app-server-subprocess-tests/Cargo.toml) and exercise the staged
  runtime, including the exec-server fixture's Linux sandbox helper so it can
  find the bundled `bwrap` without a system installation. The native development
  profile favors incremental rebuilds.

## Execution and model guidance

- Native [feature policy](codex-rs/features/src/lib.rs) fixes JavaScript code
  mode as the only model tool interface, with native V2 agents, token budgeting,
  step model switching, and reasoning-effort overrides. Stock direct tool mode
  is unsupported. Goals, Apps, agent message boards, Guardian V3, and the stock
  sleep tool are disabled; Codex feedback operations are unsupported.
- The fork owns default model instructions even when capabilities come from a
  remote catalog. Personality comes from user instructions. Native guidance
  describes available Mekugi shell helpers and can guide substantial child
  replies through `child_reply_summary`. Plugin discovery is explicit rather
  than advertised automatically in model context. The managed host catalog uses
  concise base skill guidance. Code-mode batching lives in the exec description.
- Code-mode waits can hold the observer while a native wait pauses the cell
  deadline. The paired host and runtime exchange observation liveness separately
  from stream heartbeats. Provider retry changes surface the first capacity wait,
  can resume a usage-limited turn after reset or retry advice, format multi-hour
  and multi-day waits in hours/days, and fall back to default `x-codex-*` window
  headers when `x-codex-active-limit` has no dedicated window prefix.

## Context and retained sessions

- Context resets rebuild from current sources without a generated summary.
  `new_context` can reload selected files, skills, and captured changes. Manual
  resets are cancellable and report settlement; reset receipts retain which
  sources were acquired. Accepted user input and agent handoffs can also attach
  sources through the same native acquisition path.
- The runtime captures structured environment facts, prepares the shared
  `SESSION_TMP`, and appends it to `PATH` (reported via `on_path="true"` on
  `<session_tmp>`). The native skills extension uses the local `skills-mgr`
  catalog where configured and otherwise retains stock skill discovery.
- Model requests reuse repeated tool output and source text without replacing
  the original retained records. Context usage notices and deduplication savings
  come from native observations instead of host prompt rewriting. Mekugi only
  promises resume for Mekugi sessions.
- Migration 63 records completed compactions in Mekugi state. The upstream
  0.171.0 state migrations were moved to 64 and 65 to preserve the recorded
  checksum in existing Mekugi databases.

## Native collaboration and protocol

- V2 delegation supports recipient-authorized preload, agent and subtree
  inspection, and foreign messages routed through their original resource
  owner. Side questions and progress summaries use a tool-free auxiliary
  session. Native waits support any/all, yield progress, and are separate from
  context resets.
- Native delivery keeps accepted outbound mail separate from model history and
  preserves inbound plaintext mail for live and resumed transcripts. Progress
  and final replies can attach retained change ranges; delivery receipts
  distinguish what was rendered and sampled in the recipient's context.
- The [JSON-RPC protocol](codex-rs/app-server-protocol/src/protocol/common.rs)
  adds thread inspection and subtree listing, agent end and foreign routing,
  context-reset cancellation and events, mid-turn `beforeItemId` revert targets
  on `thread/revert`, and feature-use reads. Host callbacks cover child
  admission and retained capture. Native observation also supplies assignment,
  usage, and progress facts to Mekugi without a second agent registry.

## Upstream imports

| Release | Fork commit |
| --- | --- |
| `rust-v0.171.0` | `221fbc5eed` |
| `rust-v0.170.0` | `a9129794eb` |
| `rust-v0.169.1` | `518e161a8d` |
| `rust-v0.169.0` | Initial upstream baseline |
