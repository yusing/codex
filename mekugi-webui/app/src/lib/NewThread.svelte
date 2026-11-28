<script lang="ts">
  import Alert from "./ui/Alert.svelte";
  import { onDestroy, untrack } from "svelte";
  import Attachments from "./Attachments.svelte";
  import {
    activeAttachments,
    prepareAttachments,
    resolveDeferredPathAttachments,
    submissionAttachments,
    uploadBytes,
    uploadLimit,
    type InitialInput,
    type PendingAttachment,
  } from "./attachments";
  import DraftField from "./DraftField.svelte";
  import { basename } from "./format";
  import { hub, type Machine } from "./hub.svelte";
  import type { Start, StartOptions } from "./protocol";
  import Button from "./ui/Button.svelte";
  import ComposerCard from "./ui/ComposerCard.svelte";
  import * as InputGroup from "./ui/input-group";
  import Menu from "./ui/Menu.svelte";
  import MenuItem from "./ui/MenuItem.svelte";
  import Picker from "./ui/Picker.svelte";
  import { chip, composerText, composerTools } from "./ui/styles";
  import { open, select } from "./view.svelte";

  // A new thread starts from its first message. The toolbar picks where it
  // runs and with which model.
  let {
    machine,
    onopened,
  }: {
    machine: Machine;
    onopened: (machine: string, initial: InitialInput) => void;
  } = $props();
  // The latest acquisition of a machine's start options. A failure keeps its
  // machine identity, so only Retry reads again; a superseded read is dropped.
  let acquired = $state.raw<{ machine: string; options?: StartOptions; error?: string }>();
  const options = $derived(acquired?.machine === machine.id ? acquired.options : undefined);
  const loadError = $derived(acquired?.machine === machine.id ? acquired.error : undefined);
  const workspaces = $derived.by(() => {
    const paths = new Set<string>();
    for (const session of machine.sessions ?? []) {
      if (session.workspace) paths.add(session.workspace);
      if (paths.size === 20) break;
    }
    return [...paths];
  });
  let selectedWorkspace = $state<string>();
  const workspace = $derived(selectedWorkspace ?? workspaces[0] ?? "");
  const workspaceAdmitted = $derived(workspaces.includes(workspace));
  let model = $state("");
  let effort = $state("");
  let text = $state("");
  let items = $state<PendingAttachment[]>([]);
  let pathing = $state(false);
  let error = $state<string>();
  let progress = $state("");
  let controller = $state<AbortController>();
  let tray = $state<ReturnType<typeof Attachments>>();
  let disposed = false;
  const touch = matchMedia("(pointer: coarse)").matches;
  const chosen = $derived(
    options?.models.flatMap((group) => group.models).find((choice) => choice.id === model),
  );
  const connected = $derived(machine.connection === "open");
  const busy = $derived(!!controller);
  const activeItems = $derived(activeAttachments(text, items));
  onDestroy(() => {
    disposed = true;
    controller?.abort();
  });

  // Models are loaded once per machine; workspace choices follow its thread list.
  $effect(() => {
    const target = machine;
    if (acquired?.machine !== target.id) {
      acquired = undefined;
      selectedWorkspace = undefined;
    }
    if (target.connection !== "open" || acquired) return;
    untrack(() => load(target));
  });
  async function load(target: Machine) {
    const acquisition = { machine: target.id };
    acquired = acquisition;
    const result = await hub.startOptions(target);
    if (disposed || machine.id !== target.id || acquired !== acquisition) return;
    if ("error" in result) {
      acquired = { ...acquisition, error: result.error };
      return;
    }
    acquired = { ...acquisition, options: result.options };
    model = result.options.defaultModel;
    effort = "";
  }

  async function start() {
    if (controller || !connected || tray?.checkingPath()) return;
    const pending = new AbortController();
    controller = pending;
    error = undefined;
    const target = machine;
    try {
      if (!options) throw new Error(`Wait for ${target.label}'s workspaces and models.`);
      if (!text.trim() && !activeItems.length)
        throw new Error("Write a first message or attach something.");
      const submitted = submissionAttachments(text, items);
      const hasDeferred = submitted.some((item) => "path" in item && item.deferred);
      const request: Start = {
        workspace,
        model,
        effort: effort || undefined,
      };
      const openStarted = (
        started: InitialInput["started"],
        draft: Omit<InitialInput, "started">,
      ) => {
        onopened(target.id, { started, ...draft });
        select(target.id);
        open(started.thread.id);
      };
      const draftFailureMessage = (failure: unknown, fallback: string) =>
        pending.signal.aborted
          ? "Cancelled. Edit or resend your draft in the new thread."
          : failure instanceof Error
            ? failure.message
            : fallback;
      const updateReadProgress = (read: number, total: number) => {
        progress = total
          ? `Reading attachments: ${Math.round((read / total) * 100)}%`
          : "Preparing input…";
      };
      if (hasDeferred) {
        if (uploadBytes(submitted) > uploadLimit)
          throw new Error("Uploads exceed the 32 MiB limit. Remove files and try again.");
        let draftText = text;
        let draftItems = [...submitted];
        pending.signal.throwIfAborted();
        progress = "Starting the thread…";
        const result = await hub.createSession(request, target, pending.signal);
        if (disposed) return;
        if ("error" in result) {
          error = result.error;
          return;
        }
        let failureFallback = "Could not check attachments.";
        try {
          progress = "Checking attachments…";
          const resolved = await resolveDeferredPathAttachments(
            { machine: target, thread: result.response.thread.id },
            draftText,
            draftItems,
            pending.signal,
          );
          if (disposed) return;
          draftText = resolved.text;
          draftItems = resolved.items;
          if (!resolved.ok) {
            openStarted(result.response, {
              text: draftText,
              items: draftItems,
              attachments: [],
              draftError: resolved.error,
            });
            return;
          }
          failureFallback = "Could not prepare attachments.";
          const attachments = await prepareAttachments(
            submissionAttachments(draftText, draftItems),
            pending.signal,
            updateReadProgress,
          );
          if (disposed) return;
          openStarted(result.response, { text: draftText, items: draftItems, attachments });
        } catch (failure) {
          if (disposed) return;
          openStarted(result.response, {
            text: draftText,
            items: draftItems,
            attachments: [],
            draftError: draftFailureMessage(failure, failureFallback),
          });
        }
        return;
      }
      const attachments = await prepareAttachments(submitted, pending.signal, updateReadProgress);
      const draft = { text, items: [...items], attachments };
      pending.signal.throwIfAborted();
      progress = "Starting the thread…";
      const result = await hub.createSession(request, target, pending.signal);
      if (disposed) return;
      if ("error" in result) {
        error = result.error;
        return;
      }
      openStarted(result.response, draft);
    } catch (failure) {
      if (!disposed)
        error = pending.signal.aborted
          ? "Cancelled. Work the machine already admitted may continue."
          : failure instanceof Error
            ? failure.message
            : "Could not prepare attachments.";
    } finally {
      controller = undefined;
      progress = "";
    }
  }
  function onkeydown(event: KeyboardEvent) {
    if (
      event.key === "Enter" &&
      !event.shiftKey &&
      !event.isComposing &&
      event.keyCode !== 229 &&
      !touch
    ) {
      event.preventDefault();
      void start();
    }
  }
  const ready = $derived(
    connected &&
      !busy &&
      !tray?.checkingPath() &&
      !!options &&
      !!workspace.trim() &&
      (!!text.trim() || !!activeItems.length),
  );
  // On phones the workspace picker takes its own row and the others share
  // the next one with the menu and Start.
</script>

<section
  class="grid min-h-0 flex-1 grid-cols-[minmax(0,1fr)] content-center overflow-y-auto px-4 pt-6 pb-[14dvh] max-sm:content-end max-sm:px-2 max-sm:pt-4 max-sm:pb-safe-2"
  aria-labelledby="new-heading"
>
  <div class="mx-auto w-full max-w-column">
    <div class="mb-6 flex flex-col items-center text-center max-sm:mb-auto max-sm:pb-6">
      <h1
        id="new-heading"
        class="text-heading font-medium tracking-[-0.025em] text-ink focus:outline-none"
        tabindex="-1"
      >
        What should main work on?
      </h1>
    </div>
    <ComposerCard
      accepts={!busy}
      onfiles={(files) => tray?.add(files)}
      onsubmit={() => void start()}
    >
      <Attachments
        bind:this={tray}
        bind:items
        bind:text
        bind:pathing
        machine={machine.label}
        previewTarget={{ machine, workspace, deferUnadmitted: !workspaceAdmitted }}
        disabled={busy}
      />
      <DraftField
        id="new-text"
        label="First message"
        class="{composerText} min-h-20 pt-3.5 pb-1.5"
        bind:value={text}
        {items}
        {onkeydown}
        onpaste={(event) => {
          const files = event.clipboardData?.files;
          if (!files?.length || busy) return;
          if (!event.clipboardData?.types.includes("text/plain")) event.preventDefault();
          tray?.add(files);
        }}
        disabled={busy}
        placeholder={`Message main on ${machine.label}`}
      />
      <InputGroup.Addon
        align="block-end"
        class="{composerTools} cursor-default flex-wrap gap-1 text-body font-normal select-auto"
      >
        <Menu label="More ways to start" icon="plus" disabled={busy}>
          {#snippet children(close)}
            <MenuItem
              icon="attach"
              onclick={() => {
                close();
                tray?.pick();
              }}>Attach files</MenuItem
            >
            <MenuItem
              icon="folder"
              onclick={() => {
                close();
                pathing = true;
              }}>Attach path on {machine.label}</MenuItem
            >
          {/snippet}
        </Menu>
        <Picker
          class="max-sm:flex-1"
          label="Machine"
          title="Execution machine"
          icon="server"
          value={machine.id}
          groups={[
            {
              choices: hub.machines.map((candidate) => ({
                value: candidate.id,
                label: candidate.label,
                tone: candidate.state.tone || "idle",
              })),
            },
          ]}
          disabled={busy}
          onchange={select}
        />
        {#if options}
          <!-- A path shows its folder name; the list and title show it whole. -->
          <Picker
            class="max-sm:order-first max-sm:basis-full"
            label="Workspace on {machine.label}"
            title={workspace}
            icon="folder"
            entry="Workspace path"
            path
            text={basename(workspace) || "Choose a workspace"}
            bind:value={() => workspace, (path) => (selectedWorkspace = path)}
            groups={[
              {
                choices: workspaces.map((path) => ({
                  value: path,
                  label: basename(path),
                  detail: path,
                })),
              },
            ]}
            disabled={busy}
          />
          <Picker
            class="max-sm:flex-1"
            label="Model"
            bind:value={model}
            groups={options.models.map((group) => ({
              label: group.label,
              choices: group.models.map((choice) => ({ value: choice.id, label: choice.label })),
            }))}
            disabled={busy}
            onchange={() => (effort = "")}
          />
          {#if chosen?.efforts.length}
            <Picker
              class="max-sm:flex-1"
              label="Reasoning effort"
              bind:value={effort}
              groups={[
                {
                  choices: [
                    {
                      value: "",
                      label: chosen.defaultEffort
                        ? `${chosen.defaultEffort} (default)`
                        : "Default effort",
                    },
                    ...chosen.efforts
                      .filter((value) => value !== chosen.defaultEffort)
                      .map((value) => ({ value, label: value })),
                  ],
                },
              ]}
              disabled={busy}
            />
          {/if}
        {:else if connected && !loadError}
          <span class="{chip} border-dashed" role="status">Loading workspaces and models…</span>
        {/if}
        <span class="flex-1 max-sm:hidden"></span>
        {#if busy}<Button variant="quiet" onclick={() => controller?.abort()}>Cancel</Button>{/if}
        <Button
          type="submit"
          variant="primary"
          shape="round"
          icon="send"
          label="Start thread"
          disabled={!ready}
        />
      </InputGroup.Addon>
    </ComposerCard>
    <div class="min-h-8 px-1 pt-2.5 text-small" aria-live="polite">
      {#if error}<Alert>{error}</Alert>
      {:else if progress}<p class="text-muted">{progress}</p>
      {:else if !connected}<p
          class={machine.state.tone === "alert" ? "text-negative" : "text-muted"}
        >
          {machine.label}: {machine.state.text}. {machine.error ?? ""}
        </p>
      {:else if loadError}<p class="text-negative">
          {loadError}
          <button type="button" class="underline" onclick={() => load(machine)}>Retry</button>
        </p>
      {:else if options?.note}<p class="text-muted">{options.note}</p>{/if}
    </div>
  </div>
</section>
