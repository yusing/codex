# Web UI

## Requirements

- Create, follow, and control threads across multiple explicitly selected execution machines with
  no default/implicit connection; the UI host may run separately from execution machines.
- Consume the reusable [Session API](../mekugi-tui/apps/mekugi/internal/sessionapi/CONTRACT.md) with terminal semantic-control and
  dialog parity.
- Support browser file uploads and execution-machine path attachments within the 32 MiB
  decoded-upload budget per message, with image previews.
- Edit composer attachment placeholders as indivisible units.
- Preserve queued edits across browser and native clients, including retraction for editing and
  steering queued input without losing text or attachments.
- Keep pickers, commands, live diffs, saved changes, images, and shell highlighting responsive on
  desktop and phone layouts (light and dark), showing worktree directory names and omitting
  expansion controls on empty items.
- Offer a browser display setting for disabling Live Diff without disabling Saved Diff.
- Show Main/delegate public reasoning summaries, spawn assignments, and per-agent usage/cost
  without raw reasoning or duplicate completion entries.
- Keep work server-owned: closing the browser or connector leaves threads running; ending a thread
  is explicit.

## Current availability gap

Secret prompts (`sudo`/GPG) and native-local filesystem/editor actions remain native-terminal-only.

## Current approach (replaceable)

- `mekugi web` serves embedded Svelte assets built with `just webui` (503 if
  unbuilt). It holds no machine credentials, runtimes, or proxy routes.
  Browser connections go directly to each machine's
  [Connector](../mekugi-tui/apps/mekugi/internal/connector/SPEC.md)
  over the [Web UI contract](CONTRACT.md).
- **Machines & PWA**: machine names, connector URLs, and bearer tokens store in origin-scoped
  browser storage (never URLs/cookies); installable as an online standalone PWA over HTTPS or
  localhost.
- **Chat layout & controls**: sidebar lists threads per machine; composer shares `/`, `@`, `$`,
  `!`, presets, model/effort/Fast pickers, inline selection Reference, active-process bar with Stop,
  and Retry/Edit/Discard recovery for rejected submissions. Right panel (sheet on phone) hosts
  Agents roster, Changes (per-caller composed diffs), and Details; live edits stream
  beside the transcript, or below it in a narrow pane, with independent per-card scrolling.
- See [Server, connector and browser guide](../docs/guide/remote.md) for deployment and
  Docker Compose setup.
