# Mekugi native fork changelog

This is the current inventory of intentional differences from stock Codex.
It covers the native runtime Mekugi ships, not the host UI or changes already
in upstream Codex.

The fork started at `rust-v0.169.0`. The current comparison baseline is
`rust-v0.172.0` (imported atop PR #10). Upstream imports are squashes.
Compare the release tree directly with the fork tree, not their Git merge base:

```sh
git diff rust-v0.172.0 HEAD
git log --first-parent --oneline rust-v0.169.0..HEAD
```

## Runtime and distribution

- The app-server binary is named `mekugi-app-server`. The shipped runtime also
  includes `codex-code-mode-host` and, on Linux, `bwrap`. Upstream clients and
  SDKs remain in the source tree but are not shipped or maintained as part of
  Mekugi. The [Cargo workspace](codex-rs/Cargo.toml) and lockfile cover the
  native build. Mekugi removes the upstream Bazel build, test and release
  infrastructure and uses Cargo with Mekugi's release process.
- `codex-arg0` dispatches the native Rust `mcat`, `mchanges`, `mfinspect`,
  `msymbol`, and `mrun` CLIs (`codex-mekugi-tools`) through stock PATH symlinks
  and exports
  `CODEX_ARG0_PATH_DIR` so login shells retain those aliases on `PATH`. It also
  dispatches `--codex-exec-observe` for process-scoped OS write observation
  (Linux `seccomp-unotify`, macOS `DYLD_INSERT_LIBRARIES`), replacing Bash AST
  write-path heuristics and FIFO script rewriting. Interactive user-shell (`!`)
  edits emit unattributed `shellN` change IDs (excluded from agent `--mine` and
  handoffs), and ephemeral temp-path writes (`/tmp`, `$TMPDIR`, `$SESSION_TMP`)
  are classified as `ChangeScope::Ephemeral` so they remain inspectable by
  explicit change ID without polluting default agent history or Saved Diff.
- `codex-arg0` also
  stages per-session `vcs-guard/` (`git`, `gh`, `hg`, `svn`, `jj`) and
  `sudo-guard/` (`sudo`, `gpg`, `gpg2`, `mekugi-sudo-askpass`) symlinks
  dispatched by `codex-shell-guard` without adding them to the process-wide
  `PATH`. When `mrun` owns a controlling PTY foreground process group, it hands
  off and restores foreground terminal ownership for the wrapped process group
  (propagating `SIGINT`/`SIGQUIT` and `SIGTSTP`/`SIGCONT`) so terminal stdin
  reads complete without losing whole-group cleanup.
- Native shell guarding (`codex-shell-guard`, gated by `features.shell_guard`,
  default enabled in interactive sessions and disabled for headless/internal
  runs) prepends the staged guard directories in `codex-core` after user
  `PreToolUse` hooks run and applies a minimal `tree-sitter-bash` executable-word
  rewriter for explicit paths, `env -i`, `command -p`, `PATH` overrides, and
  nested `-c` shells without a patched shell or `BASH_ENV` function injection,
  preserving quoted here-document bodies verbatim across list and pipeline
  operators (`MEKUGI-825`).
  The session tracking hook instruments the command selected by user hooks;
  competing user rewrites still resolve in native completion order.
  Shims communicate over a session-private Unix socket (`codex-uds`) bridged by
  `mekugi-app-server` to App-Server Protocol v2 `shellGuard/requestApproval` and
  `shellGuard/requestSecret` requests, keeping secrets zeroized and excluded
  from rollouts, tracing, and analytics.
- Command items optionally include authored shell steps and a resolved display
  directory. The original command, launch directory, exit and output remain
  unchanged; individual steps do not claim separate execution results.
  The native projection labels recognized steps, and clients show the shared
  result of a multi-step invocation in Details only. Unified exec streams
  startup output after the command-start event.
- Native coordination uses the shared thread manager for live foreign messages,
  side questions, listing and inspection. Child startup, wait-progress selection
  and final change delivery no longer request resources from a connected Go
  client. Preload and context reset resolve retained change IDs from the
  native store without a launcher opt-in. The retired resource
  callbacks are no longer on the v2 wire.
- Native change-evidence capture (`codex-change-history`, `MEKUGI-806`):
  `codex-core` captures pre-edit baseline bytes and post-edit observed bytes at
  the `apply_patch` tool boundary (covering direct calls, intercepted shell
  patches, partial writes before failing exits, and JavaScript code-mode `exec`
  cells), persists immutable content-addressed blobs and monotonic stream-scoped
  change records (`amber1`, ...) before publishing tool receipts (surfacing
  storage failures explicitly), exports `MEKUGI_CHANGE_HISTORY_DIR` to child
  shells, and backs the native `mchanges` CLI (`--list`, composed diff review,
  `--summary`, `--history`, and path-scoped `revert`/`apply` preserving
  unrelated edits and failing explicitly on drift or corrupt/missing evidence
  without conflict markers). Composed range diffs, summaries, and workspace
  mutations omit continuous net-zero transitions (`before == after` without
  rename or incomplete coverage) and surface discontinuous baselines from
  unobserved external writes via coverage notes (`MEKUGI-827`).
- Native app-server presentation (`MEKUGI-807`): `mekugi-app-server` exposes
  `thread/changes/read` over App-Server Protocol v2 so direct Unix-socket
  clients (including the Go TUI connecting to `--listen unix://PATH`) read
  durable saved changes and composed diffs directly from `codex-change-history`
  without an intermediate Go session server.
- The browser dock and terminal process picker read native per-thread background
  terminals. The experimental change notification invalidates the list after
  registration or exit, including for observers not subscribed to a delegate.
  It carries only the thread ID, not command output.
- Native process-scoped shell write observation (`codex-exec-track`,
  `MEKUGI-801`): `codex-core` wraps agent and interactive `!` user-shell
  commands with `WriteObserverSession` (Linux `seccomp-unotify` and macOS
  `DYLD_INSERT_LIBRARIES`), capturing real filesystem writes lazily before
  mutation without Bash AST parsing or shell script rewriting, preserving TTYs
  intact, working on non-git directories, and capturing writes outside `cwd`
  (classifying `/tmp`, `$TMPDIR`, and `$SESSION_TMP` as
  `ChangeScope::Ephemeral`). On Apple Silicon the interposer loads in both
  `arm64` and `arm64e` child processes. Observed writes before nonzero
  exits also persist to `codex-change-history` under `ChangeSourceKind::ShellWrite`
  for agent commands and `ChangeSourceKind::UserShell` (`"user_shell"`,
  unattributed `shellN`) for interactive `!` commands with shared `claim_key`
  deduplication, while classifying Git worktree restores on Linux (`git restore`,
  `git checkout --`, `git reset --hard`, `git clean`) under
  `ChangeSourceKind::VcsDiagnostic` (`MEKUGI-827`). When an older session is
  reopened, worktree-only setup and cleanup no longer count as authored edits;
  captures that also contain unrelated writes stay visible. Downgrade note:
  older runtimes deserialize `ChangeSourceKind` strictly and will reject a
  session's `change-history/index.json` once a `user_shell` entry is recorded.
- Subprocess regression tests are in a separate
  [crate](codex-rs/app-server-subprocess-tests/Cargo.toml) and exercise the staged
  runtime, including the exec-server fixture's Linux sandbox helper so it can
  find the bundled `bwrap` without a system installation. The native development
  profile favors incremental rebuilds.

## Execution and model guidance

- Both question tools share rich options (preview and explicit recommendation) and
  optional headings/free-text-only questions. Runtime-assigned qN IDs continue
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
  than advertised automatically in model context. The managed host catalog uses
  concise base skill guidance. Code-mode batching lives in the exec description.
- Exec guidance tells agents to summarize or interpret visible command output
  without copying it into replies unless requested there.
- Code-mode shell results expose `event_call_id` for Markdown run links. These
  open retained command output in the terminal or browser. Run links, local
  file links, and source references share `#L12` and `#L12-L18` locations.
  Existing colon-suffixed links still work.
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
- Live Diff streams authored edits as the model generates tool call inputs
  (`apply_patch`, `exec_command`, and nested Code Mode `exec` calls writing
  via `cat`/`tee` heredocs or Python `open`/`read_text` -> `replace` ->
  `write_text`/`write`) through `item/fileChange/patchUpdated`. Execution-time
  OS polling (`item/commandExecution/changesUpdated`) is retired; for
  compatibility, `thread/changes/live` remains callable and returns empty,
  while Saved Diff reads settled `exec-track` captures for all commands.
- When session naming is enabled, new persistent Main threads can receive a
  native title from a one-shot, untooled request carrying `thread_title` turn
  metadata (`gpt-6-luna` on built-in OpenAI, skipping local spend/budget
  accounting under ChatGPT auth; selected model on custom providers). An
  existing manual name wins; title failure does not fail the turn.
- Native request construction expands submitted attachment snapshots and
  attributes standalone tool context without rewriting retained review items.
  Side forks reuse parent cache routing; answer-only forks stop when auto
  reset is required. UI BTW caps supported effort at low. Native exact-model
  tier defaults and thread choices use provider capability checks. Resuming
  a stopped session keeps explicit choices and uses current defaults otherwise.

- Context resets rebuild from current sources without a generated summary.
  Retained app-server items distinguish these resets from compaction and include
  a plaintext summary only when a local compaction generated one.
  `new_context` can reload selected files, skills, and captured changes. Manual
  resets are cancellable and report settlement; reset receipts retain which
  sources were acquired. Accepted user input and agent handoffs can also attach
  sources through the same native acquisition path.
- In both clients, `/compact` with Enter replaces active work and Tab defers
  until idle. A successful busy-thread compact continues without a synthetic
  prompt when no queued draft takes precedence. The experimental
  `thread/compact/start.mode` opts in; existing compact and reset calls keep
  their behavior.
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
  Both clients manage shared user preset slots through `thread/presets/list`
  and `thread/presets/write`, without access to the full machine configuration.
  Terminals can save drafts to Codex input history without submitting a turn
  and recall them from a new attachment.
- V2 delegation supports recipient-authorized preload, agent and subtree
  inspection, and foreign messages routed through their original resource
  owner. Side questions and progress summaries use a tool-free auxiliary
  session. Native waits support any/all, yield progress, and are separate from
  context resets.
- Native delivery keeps accepted outbound mail separate from model history and
  preserves inbound plaintext mail for live and resumed transcripts. Progress
  and final replies can attach retained change ranges; delivery receipts
  distinguish what was rendered and sampled in the recipient's context. The
  transcript shows the author, recipient, and authored reply separately from
  model-only attachment context.
- The [JSON-RPC protocol](codex-rs/app-server-protocol/src/protocol/common.rs)
  adds thread inspection and subtree listing, agent end,
  context-reset cancellation and events, mid-turn `beforeItemId` revert targets
  on `thread/revert`, and feature-use reads. Native observation supplies assignment,
  usage, and progress facts to Mekugi without a second agent registry.
  When the user opts in to browser predictions, `thread/prediction/request`
  samples the selected finished turn without adding it to history and returns
  the suggestion in the request's response instead of a separate notification.
  Native account eligibility gates automatic terminal suggestions.
  Rewind uses `thread/revert` for both choices. Clients can include the verified
  IDs from `thread/revert/preview` to undo captured edits without a shell turn.
  If the history cutover fails, the server restores the workspace from its undo
  record. Captures distinguish earlier kept edits even within the same second.
  The browser reads the same launch counters for the selected thread and its
  descendants. BTW and One-off use native side forks rather than a separate host
  session. One-off defaults to Main's current model and reasoning effort. The
  terminal hides its inherited transcript and shows only One-off's own usage
  and cost.
  `ThreadTokenUsage` carries per-thread reference cost (`estimatedCostUsd`,
  `costPartial`), measured output throughput (`outputTokensPerSecond`),
  cumulative work duration (`workDurationMs`), and response roundtrip count
  (`roundtrips`). Native restores known totals, coverage, turn counts, and actual
  measurement times from retained history; clients do not recompute totals.
  A fork retains its inherited context size without claiming its source
  thread's cumulative usage.
  Per-thread skills used are available on saved reads and via
  `thread/skills/updated` while attached.
  A subtree read during child startup returns its readable members without
  waiting for the child or reporting the parent missing.
- `thread/files/search` gives composers native fuzzy matches in the selected
  thread's workspace without traversing directory symlinks. VCS metadata is
  excluded even when ignored files are requested. The explicit-root fuzzy
  search interface is unchanged.
- `thread/changes/read` returns captured edit provenance, its display label,
  and signed line totals from native history. Web Saved Diff pages by caller;
  terminal Details join captured effects to the original execution. Terminal
  attachment does not wait for Saved Diff. Paginated per-capture reads bound
  each reply, pack multi-slice patches up to the page budget, preserve each
  origin's full filtered path list when it fits (falling back to per-page paths
  on oversized captures while clients merge origins across pages), and report
  line totals for the page.
  `receiptItemIds` returns per-capture file metadata and counts without patch
  text for the requested command items in the selected thread.
  Composed and caller-filtered pages keep their totals and continue at the
  next file when a reply would exceed the connector limit. An oversized
  individual patch keeps its metadata and shows a display-limit note.
  Oversized unpaged replies return an RPC error instead of closing the client.
- `thread/queue/shellCommand` places a standalone user-shell turn in queue
  order, preserving native edit capture without submitting a model prompt.
  Existing queued prompts retain their stored format.
- `thread/queue/take` returns waiting drafts in order for editing.
  The clients steer with ordinary `turn/steer` and remove each admitted draft
  with `thread/queue/delete`. Queued shell commands cannot steer the model.
- The browser can list permission profiles for a loaded thread's workspace.
  Remote clients cannot choose a different catalog directory.
- Background-terminal listings include the command's turn ID. Callers can
  resolve retained output in that turn and pass the turn and item IDs to Stop
  so a stale action cannot terminate a replacement using the same process ID.
  Older process-ID-only stop requests still work.
  A unified-exec rejection before launch publishes its native result in the
  command item, separately from closure of the approval prompt.
  Loaded thread observations report `extra.backgroundTerminalCount` so clients
  can skip eager per-member `thread/backgroundTerminals/list` lookups for
  family members with zero active terminals.
- A named turn interrupt can stop only that turn, even if it finishes and its
  replacement starts before the interrupt reaches the runtime. JSON-RPC error
  data identifies a stale interrupt target or an unloaded process-list thread.
- Command display metadata identifies the authored shell script, step subjects
  such as skill names and read targets, and resolved effective working directory
  for live and saved commands.
  Batch status and output still belong to the whole invocation, not to
  individual segments.
- Thread input-history reads and appends let the terminal save and recover
  drafts through the native input history, honoring its configured persistence.
- The private Unix control socket serves a restricted remote-human WebSocket
  endpoint (`/api/control`, negotiating `mekugi.control.v9` with
  `ConnectionOrigin::RemoteHuman`) alongside authenticated HTTP resource
  endpoints (`GET /api/version`, `POST /api/resources/{content,file,image,attachment-image}`).
  `POST /api/resources/attachment-image` confines reads and probes to canonical
  admitted workspaces (either the target thread's workspace or an existing thread
  workspace when no thread ID is supplied) and returns uniform errors for
  non-workspace or unreadable paths.
  Remote-human connections enforce authority at `mekugi-app-server`, blocking
  native startup / host process RPCs and private secret prompts (`sudo`
  password, GPG passphrase, and `is_secret` user input) while leaving active
  turns running when the connector disconnects or restarts.

- Remote-human connections can use session approval for eligible commands
  and file changes. Guarded VCS approvals remember exact arguments,
  executable, and working directory for the same thread and attachment.
  Remote policy amendments, session grants for network access or additional
  permissions, and private secret responses remain unavailable.

## Upstream imports

| Release | Fork commit |
| --- | --- |
| `rust-v0.172.0` | Pending merge (`chore/upstream-rust-v0.172.0`) |
| `rust-v0.171.0` | `221fbc5eed` |
| `rust-v0.170.0` | `a9129794eb` |
| `rust-v0.169.1` | `518e161a8d` |
| `rust-v0.169.0` | Initial upstream baseline |
