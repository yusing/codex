# Mekugi native fork changelog

This inventories the intentional native runtime, protocol, and distribution
differences from stock Codex. It excludes the host clients and changes already
in upstream Codex.

The fork started at `rust-v0.169.0`. The current comparison baseline is
`rust-v0.172.0`. Upstream imports are squashes.
Compare the release tree directly with the fork tree, not their Git merge base:

```sh
git fetch https://github.com/openai/codex.git tag rust-v0.172.0
git diff rust-v0.172.0 HEAD
```

## Runtime and distribution

- The app-server binary is named `mekugi-app-server`. The shipped runtime also
  includes `codex-code-mode-host` and Linux `bwrap`, but not upstream clients
  or SDKs.
- `codex-arg0` dispatches `mcat`, `mchanges`, `mfinspect`, `msymbol`, and
  `mrun`. It exports `CODEX_ARG0_PATH_DIR` so login shells retain the aliases
  on `PATH`.
  `mrun` hands off terminal foreground ownership so child processes can read
  terminal input without losing whole-group signal handling and cleanup.
- `features.shell_guard` enables native interactive shell guarding after user
  `PreToolUse` hooks run. Guarded VCS writes and `sudo` request approval via
  `shellGuard/requestApproval`. Sudo and GPG credentials use
  `shellGuard/requestSecret` and stay out of rollouts, tracing, and analytics.
  The guard supports explicit executable paths, `env -i`, `command -p`, `PATH`
  overrides, and nested `-c` shells while preserving quoted heredoc bodies.
  Guard aliases are not added to the process-wide `PATH`.
- Unified exec streams startup output after the command-start event.
- Native change history retains pre- and post-edit evidence for `apply_patch`
  and observed agent and user shell writes, including writes before a failed
  exit, in non-Git directories, or outside the working directory. Shell write
  capture runs on Linux and macOS, including Apple Silicon `arm64e` processes.
  Temp-path writes are excluded from default history but are addressable by
  change ID. Interactive user-shell changes use unattributed `shellN` IDs and
  are excluded from `--mine` and agent handoffs. On Linux, Git worktree
  operations and Git restores are recorded as VCS diagnostics, not authored
  edits. Saved Diff also excludes worktree-only setup and cleanup when older
  sessions reopen, but retains captures with unrelated writes. Older runtimes
  reject indexes containing the `user_shell` source kind.
- The runtime exports `MEKUGI_CHANGE_HISTORY_DIR`. `mchanges` lists and composes
  captured edits, reports gaps in observed history, and omits continuous
  net-zero edits. Its path-scoped revert and apply preserve unrelated edits and
  fail on drift or invalid evidence without inserting conflict markers.

## Execution and model guidance

- Both question tools share rich options (preview and explicit recommendation)
  and support optional headings and free-text-only questions. Runtime-assigned
  qN IDs continue
  across context resets, forks and resume. Answers are compact indexed objects;
  async replies use `<question_answers>`, without expansion after context reset.
  The v2 response now carries an ordered answer vector, not a string-answer map;
  clients must migrate. A blocking answer can carry images for its own question;
  native passes them to the model as image input. Catalog parameter overrides
  cannot replace this schema.
  Async questions decode and serialize only the canonical rich-option shape, and
  retained authored IDs do not reset the runtime question counter. The shared
  question schema instructs using option `preview` for appearance or interaction
  choices to show the same concrete example under each option in the picker, and
  `request_user_input_async` appends sequencing guidance (batch independent
  questions; apply outstanding answers before asking dependent questions) to
  both bundled and catalog-supplied descriptions.

- Tool, unified-exec, and code-mode output uses stock upstream middle
  truncation and spills the omitted portion to
  `$SESSION_TMP/truncated_output/{id}` (created on demand), adding only a short
  `(id: {id})` marker inline and stating the directory once in session shell
  helper guidance. The spill directory enforces a 64 MiB per-session
  oldest-first ring-buffer cap (keeping the newest spill intact) and is pruned
  automatically when a thread is archived or deleted.

- Native [feature policy](codex-rs/features/src/lib.rs) fixes JavaScript code
  mode as the only model tool interface, with native V2 agents, token budgeting,
  step model switching, and reasoning-effort overrides. Stock direct tool mode
  is unsupported. Goals, Apps, agent message boards, Guardian V3, and the stock
  sleep tool are disabled; Codex feedback operations are unsupported.
- The fork owns default model instructions even when capabilities come from a
  remote catalog. Personality comes from user instructions. Native guidance
  describes available Mekugi shell helpers and can guide substantial child
  replies through `child_reply_summary`. Plugin discovery is explicit rather
  than advertised automatically in model context. The managed skills catalog
  uses concise base guidance. Code-mode batching lives in the exec description.
- Exec guidance tells agents to summarize or interpret visible command output
  without copying it into replies unless requested there.
- Code-mode shell results expose `event_call_id` for linking to retained
  command output.
- Code-mode waits can hold the observer while a native wait pauses the cell
  deadline. The paired host and runtime exchange observation liveness separately
  from stream heartbeats. Instant-interrupt steering defers stream preemption
  until an in-progress assistant message completes and suppresses streaming of
  partial items while draining a steered response, preventing mid-truncated
  assistant messages. Provider retry changes surface the first capacity wait,
  can resume a usage-limited turn after reset or retry advice, format multi-hour
  and multi-day waits in hours/days, and fall back to default `x-codex-*` window
  headers when `x-codex-active-limit` has no dedicated window prefix.

## Context and retained sessions

- Forks retain inherited context occupancy but start independent provider
  spending totals, including when the source was compacted. Native response
  records retain per-thread usage, output timing and reference OpenAI costs.
  A response completed without usage leaves a coverage gap. Spending
  updates leave context occupancy unchanged.
- Mekugi sends requests directly to the selected provider. A custom route
  requires an endpoint, cannot redirect inference or catalog requests, and has
  no built-in price. Native exchange measurements feed launch-wide diagnostics
  without retaining prompts, tool input, or response text in the private log.
- The runtime streams provisional edit patches as the model generates tool input
  (`apply_patch`, `exec_command`, and nested Code Mode `exec` calls writing
  via `cat`/`tee` heredocs or Python `open`/`read_text` -> `replace` ->
  `write_text`/`write`) through `item/fileChange/patchUpdated`.
  `item/commandExecution/changesUpdated` is no longer sent.
  `thread/changes/live` remains callable and returns empty; settled evidence
  comes from `thread/changes/read`.
- When session naming is enabled, new persistent Main threads can receive a
  native title from a one-shot, untooled request carrying `thread_title` turn
  metadata (`gpt-6-luna` on built-in OpenAI, skipping local spend/budget
  accounting under ChatGPT auth; selected model on custom providers). An
  existing manual name wins; title failure does not fail the turn.
- Native request construction expands submitted attachment snapshots and
  attributes standalone tool context without rewriting retained review items.
  Side forks reuse parent cache routing; answer-only forks stop when auto
  reset is required. Native exact-model tier defaults and thread choices use
  provider capability checks. Resuming
  a stopped session keeps explicit choices and uses current defaults otherwise.

- Context resets rebuild from current sources without a generated summary.
  Retained app-server items distinguish these resets from compaction and include
  a plaintext summary only when a local compaction generated one.
  `new_context` can reload selected files, skills, and captured changes. Manual
  resets are cancellable and report settlement; reset receipts retain which
  sources were acquired. Accepted user input and agent handoffs can also attach
  sources through the same native acquisition path.
- Experimental `thread/compact/start.mode` can replace active work or queue
  compaction until idle. After a successful compact the runtime resumes the
  interrupted turn if no queued draft takes precedence. Existing compact and
  reset calls keep their behavior.
- The runtime captures structured environment facts, prepares the shared
  `SESSION_TMP`, and appends it to `PATH` (reported via `on_path="true"` on
  `<session_tmp>`). The native skills extension uses the local `skills-mgr`
  catalog where configured and otherwise retains stock skill discovery.
- Native shell capture uses the runtime's `SESSION_TMP` path to resolve
  direct output redirects without changing ordinary hook input.
- Model requests reuse repeated tool output and source text without replacing
  the original retained records, preferring a single-base unified diff over
  fragmented multi-segment references when savings stay within a bounded
  clarity budget. Detected `AGENTS.md` instruction updates are delivered as a
  compact unified diff against the previously delivered snapshot when doing so
  saves at least 128 bytes compared to repeating the full instructions (and emit
  a concise unchanged notice on directory-only changes), falling back to full
  replacement for complete rewrites or legacy sessions without a baseline.
  Context usage notices and deduplication savings come from native observations
  instead of host prompt rewriting. Mekugi only promises resume for Mekugi
  sessions.
- On cold resume, explicit model and reasoning-effort overrides replace only
  the named setting. A model change also retains an explicit saved service
  tier if the new model has neither its own saved choice nor a configured tier.
- Migration 63 records completed compactions in Mekugi state. The upstream
  0.171.0 state migrations were moved to 64 and 65 to preserve the recorded
  checksum in existing Mekugi databases.

## Native collaboration and protocol

- Native thread start and cold resume accept a private caller environment and
  named profile. Concurrent families keep separate settings and credentials;
  live reattach verifies the selector without reopening its profile.
  `thread/presets/list` and `thread/presets/write` expose shared preset slots
  without exposing the full machine configuration.
- V2 delegation supports recipient-authorized preload, agent and subtree
  inspection, and foreign messages routed through their original resource
  owner. Side questions and progress summaries use a tool-free auxiliary
  session. Native waits support any/all, yield progress, and are separate from
  context resets. Child startup, preloads, resets, and final change delivery
  resolve retained native resources without a client callback.
- Native delivery keeps accepted outbound mail separate from model history and
  preserves inbound plaintext mail for live and resumed transcripts. Progress
  and final replies can attach retained change ranges; delivery receipts
  distinguish what was rendered and sampled in the recipient's context.
- The [JSON-RPC protocol](codex-rs/app-server-protocol/src/protocol/common.rs)
  adds thread inspection and subtree listing, agent end,
  context-reset cancellation and events, mid-turn `beforeItemId` revert targets
  on `thread/revert`, and feature-use reads. Native observation supplies
  assignment, usage, and progress facts.
  `thread/prediction/request` samples the selected finished turn without
  modifying history and returns the suggestion in the response instead of
  a separate notification. Native account eligibility gates automatic
  suggestions.
  `thread/revert` accepts verified IDs from `thread/revert/preview` to undo
  captured edits without a shell turn.
  If the history cutover fails, the server restores the workspace from its undo
  record. Captures distinguish earlier kept edits even within the same second.
  Native one-off forks inherit the source model and reasoning effort by default.
  `ThreadTokenUsage` carries per-thread reference cost (`estimatedCostUsd`,
  `costPartial`), measured output throughput (`outputTokensPerSecond`),
  cumulative work duration (`workDurationMs`), and response roundtrip count
  (`roundtrips`). The runtime restores totals, coverage, turn counts, and
  measurement times from retained history.
  A fork retains its inherited context size without claiming its source
  thread's cumulative usage.
  Per-thread skills used are available on saved reads and via
  `thread/skills/updated` while attached.
  A subtree read during child startup returns its readable members without
  waiting for the child or reporting the parent missing.
- `thread/files/search` returns fuzzy matches in the selected thread's
  workspace without traversing directory symlinks. VCS metadata is
  excluded even when ignored files are requested. The explicit-root fuzzy
  search interface is unchanged.
- `thread/changes/read` returns native diffs, provenance, display labels, and
  signed line totals, with optional caller filtering. Per-capture pages can
  continue inside a patch and report totals for the page. An origin's full
  filtered path list is returned when it fits; oversized captures return
  paths per page.
  `receiptItemIds` returns per-capture file metadata and counts without patch
  text for the requested command items in the selected thread.
  Composed and caller-filtered pages keep their totals and continue at the
  next file when a reply would exceed the connector limit. An oversized
  individual patch returns metadata with a display-limit note.
  Oversized unpaged replies return an RPC error instead of closing the client.
- `thread/queue/shellCommand` places a standalone user-shell turn in queue
  order, preserving native edit capture without submitting a model prompt.
  Existing queued prompts retain their stored format.
- `thread/queue/take` returns waiting drafts in order for editing. Queued shell
  commands cannot steer the model.
- `permissionProfile/list` can use a loaded thread's workspace. Remote
  callers cannot choose a different catalog directory.
- Background-terminal listings include the command's turn ID. Callers can
  resolve retained output in that turn and pass the turn and item IDs to stop
  so a stale action cannot terminate a replacement using the same process ID.
  Older process-ID-only stop requests still work.
  A unified-exec rejection before launch publishes its native result in the
  command item, separately from closure of the approval prompt.
  A thread-ID notification reports registration or exit, including for family
  observers not subscribed to the delegate. Loaded thread observations expose
  the count in `extra.backgroundTerminalCount`.
- A named turn interrupt can stop only that turn, even if it finishes and its
  replacement starts before the interrupt reaches the runtime. JSON-RPC error
  data identifies a stale interrupt target or an unloaded process-list thread.
- Command display metadata identifies the authored shell script, step subjects
  such as skill names and read targets, and resolved effective working directory
  for live and saved commands.
  Batch status and output still belong to the whole invocation, not to
  individual segments.
- `thread/inputHistory/read` and `thread/inputHistory/append` access native
  input history without submitting a turn and honor its configured persistence.
- The private Unix control socket serves a restricted remote-human WebSocket
  endpoint (`/api/control`, negotiating `mekugi.control.v9`) alongside
  authenticated HTTP endpoints for version, content, files, and images.
  `POST /api/resources/attachment-image` confines reads and probes to canonical
  admitted workspaces: the target thread's workspace or an existing thread's
  workspace when no thread ID is supplied. Non-workspace and unreadable
  paths return uniform errors.
  Remote-human connections enforce authority at `mekugi-app-server`, blocking
  native startup / host process RPCs and private secret prompts (`sudo`
  password, GPG passphrase, and `is_secret` user input) while leaving active
  turns running when the remote connection closes.

- Remote-human connections can use session approval for eligible commands
  and file changes. Guarded VCS approvals remember exact arguments,
  executable, and working directory for the same thread and attachment.
  Remote policy amendments, session grants for network access or additional
  permissions, and private secret responses remain unavailable.

## Upstream imports

| Release | Fork commit |
| --- | --- |
| `rust-v0.172.0` | `08d4ba3328` |
| `rust-v0.171.0` | `221fbc5eed` |
| `rust-v0.170.0` | `a9129794eb` |
| `rust-v0.169.1` | `518e161a8d` |
| `rust-v0.169.0` | Initial upstream baseline |
