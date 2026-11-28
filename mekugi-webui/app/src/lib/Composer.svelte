<script lang="ts">
  import Alert from "./ui/Alert.svelte";
  import { onDestroy, tick, untrack } from "svelte";
  import Attachments from "./Attachments.svelte";
  import {
    activeAttachments,
    attachmentRanges,
    hostPathAttachment,
    labelAttachment,
    prepareAttachments,
    referenceLabel,
    resolveDeferredPathAttachments,
    submissionAttachments,
    type InitialInput,
    type PendingAttachment,
  } from "./attachments";
  import Code from "./Code.svelte";
  import ComposerPresets from "./ComposerPresets.svelte";
  import DraftField from "./DraftField.svelte";
  import type { Machine } from "./hub.svelte";
  import Path from "./Path.svelte";
  import type {
    Attachment,
    ComposerPreset,
    Input,
    SkillsListResponse,
    ThreadFileSearchResponse,
    ThreadPresetsListResponse,
  } from "./protocol";
  import Rewind from "./Rewind.svelte";
  import { semantic, skillsAfterEdit, SLASH_COMMANDS } from "./semantic.svelte";
  import { streams } from "./stream.svelte";
  import { errorText, newIdentity } from "./transport";
  import ThreadSettings from "./ThreadSettings.svelte";
  import Button from "./ui/Button.svelte";
  import ComposerCard from "./ui/ComposerCard.svelte";
  import Dialog from "./ui/Dialog.svelte";
  import * as InputGroup from "./ui/input-group";
  import Menu from "./ui/Menu.svelte";
  import MenuItem from "./ui/MenuItem.svelte";
  import Tag from "./ui/Tag.svelte";
  import { composerText, composerTools, kbd } from "./ui/styles";

  let {
    machine,
    thread,
    input,
    agent = "/root",
    role,
    initial,
    oninitial,
    oneOffOpen,
    inOneOff,
    ononeoff,
    onarchive,
    asking = false,
    unavailable,
  }: {
    machine: Machine;
    thread: string;
    input?: Input;
    agent?: string;
    role?: string;
    initial?: InitialInput;
    oninitial: () => void;
    oneOffOpen: boolean;
    inOneOff: boolean;
    ononeoff: (draft: Omit<InitialInput, "started" | "onstarted">) => Promise<string | undefined>;
    onarchive: () => Promise<string | undefined>;
    asking?: boolean;
    unavailable?: string;
  } = $props();

  const stream = $derived(streams.current);
  let editor = $derived(semantic.editor(thread));
  let text = $state("");
  let items = $state<PendingAttachment[]>([]);
  let pathing = $state(false);
  let sending = $state(false);
  let progress = $state("");
  let error = $state<string>();
  let settingsError = $state<string>();
  let tray = $state<ReturnType<typeof Attachments>>();
  let field = $state<ReturnType<typeof DraftField>>();
  let syntax = $state<HTMLElement>();
  let selectedText = $state(false);
  let controller = $state<AbortController>();
  let fileRequest = $state<AbortController>();
  let archiveTarget = $state<{ thread: string; pending: boolean; error?: string }>();
  let rewindThread = $state<string>();
  let replacing = $state(false);
  let presetDialog = $state<{ thread: string; machine: Machine; seed: string }>();
  let presetCatalog = $state.raw<{
    thread: string;
    machine: Machine;
    slots: ThreadPresetsListResponse["presets"];
  }>();
  let presetError = $state<string>();
  let readingPresets = $state(false);
  const touch = matchMedia("(pointer: coarse)").matches;
  const shell = $derived(text.startsWith("!"));
  const bareShell = $derived(text === "!");
  const highlightShell = $derived(shell && !bareShell && !selectedText);
  const addressee = $derived(inOneOff ? "One-off" : stream.label(agent));
  const shellText = "whitespace-pre-wrap wrap-break-word font-mono [scrollbar-gutter:stable]";

  $effect(() => {
    const current = editor;
    untrack(() => {
      text = current.text;
      items = current.items;
      archiveTarget = undefined;
      replacing = false;
      presetDialog = undefined;
      fileRequest?.abort();
      fileRequest = undefined;
    });
  });

  $effect(() => {
    editor.skills = skillsAfterEdit(editor.text, text, editor.skills);
    editor.text = text;
    editor.items = items;
  });

  // Deliver the first message once the new stream and input target are ready.
  $effect(() => {
    const first = initial;
    const target = first?.started.thread.id;
    if (
      !first ||
      sending ||
      fileRequest ||
      target !== thread ||
      stream.session?.thread !== target ||
      semantic.threadId !== target ||
      semantic.connection !== "open" ||
      unavailable
    )
      return;
    untrack(() => {
      oninitial();
      text = first.text;
      items = [...first.items];
      editor.text = first.text;
      editor.skills = first.skills ?? [];
      if (first.draftError) {
        error = first.draftError;
        return;
      }
      void send(false, first.attachments, false, first.onstarted);
    });
  });

  onDestroy(() => {
    controller?.abort();
    fileRequest?.abort();
  });

  const disabled = $derived(!!unavailable || sending || semantic.connection !== "open");
  const stoppable = $derived(!!input?.stoppable || !!stream.activeTurnId);
  const prediction = $derived(
    !inOneOff && !text && !disabled && !asking && !stoppable ? semantic.prediction : undefined,
  );
  const activeItems = $derived(activeAttachments(text, items));
  const presetsAvailable = $derived(
    !inOneOff &&
      !asking &&
      agent === "/root" &&
      stream.session?.thread === thread &&
      stream.family?.root === thread,
  );
  const presetOpen = $derived(
    presetsAvailable && presetDialog?.thread === thread && presetDialog.machine === machine,
  );
  const presetSlots = $derived(
    presetCatalog?.thread === thread && presetCatalog.machine === machine
      ? presetCatalog.slots
      : undefined,
  );
  const canSend = $derived(
    !disabled && !fileRequest && !tray?.checkingPath() && (!!text.trim() || !!activeItems.length),
  );

  type CompletionChoice = {
    name: string;
    description: string;
    insert: string;
    start: number;
    end: number;
    path?: string;
    skill?: { name: string; path: string };
    preset?: string;
    display?: string;
  };

  let skillCatalog = $state.raw<
    { name: string; path: string; display: string; description: string }[]
  >([]);
  let skillRevision = $state(0);
  let menuDismissed = $state(false);
  let menuIndex = $state(0);
  let caret = $state<number>();
  let fileChoices = $state.raw<{ thread: string; query: string; paths: string[] }>();

  const completionTarget = $derived.by(() => {
    if (shell) return;
    if (text.startsWith("/") && !/\s/.test(text) && text.indexOf("/", 1) < 0) {
      return { kind: "/" as const, query: text.slice(1).toLowerCase(), start: 0, end: text.length };
    }
    const at = Math.min(caret ?? text.length, text.length);
    const match = /(?:^|\s)([@$][^\s]*)$/.exec(text.slice(0, at));
    if (match) {
      const token = match[1];
      const kind = token.startsWith("@") ? "@" : "$";
      const query = token.slice(1);
      const start = at - token.length;
      const end = at + (text.slice(at).match(/^\S*/)?.[0].length ?? 0);
      if (attachmentRanges(text, items).some((range) => range.start < end && range.end > start))
        return;
      return {
        kind,
        query: kind === "@" ? query : query.toLowerCase(),
        start,
        end,
      };
    }
    return undefined;
  });

  $effect(() =>
    machine.onNotification(({ method }) => {
      if (method === "skills/changed") skillRevision++;
    }),
  );

  const skillThread = $derived(stream.session?.thread);
  const skillWorkspace = $derived(stream.session?.workspace);
  const wantsSkills = $derived(completionTarget?.kind === "$");
  $effect(() => {
    void skillRevision;
    skillCatalog = [];
    if (
      !wantsSkills ||
      machine.connection !== "open" ||
      stream.machine !== machine ||
      skillThread !== thread
    )
      return;
    const cwd = skillWorkspace;
    const params = cwd ? { cwds: [cwd] } : {};
    const catalogRequest = new AbortController();
    void machine
      .request<SkillsListResponse>("skills/list", params, { signal: catalogRequest.signal })
      .then((res) => {
        if (catalogRequest.signal.aborted) return;
        const seen = new Set<string>();
        const loaded: typeof skillCatalog = [];
        for (const bucket of res.data ?? []) {
          for (const skill of bucket.skills ?? []) {
            if (!skill.enabled || !skill.name || !skill.path || seen.has(skill.path)) continue;
            seen.add(skill.path);
            loaded.push({
              name: skill.name,
              path: skill.path,
              display: skill.interface?.displayName || `$${skill.name}`,
              description:
                skill.interface?.shortDescription ||
                skill.shortDescription ||
                skill.description ||
                "",
            });
          }
        }
        skillCatalog = loaded;
      })
      .catch(() => {});
    return () => catalogRequest.abort();
  });

  const wantsPresets = $derived(presetOpen || completionTarget?.kind === "$");
  $effect(() => {
    const owner = machine;
    const threadId = thread;
    void presetOpen; // Opening management refreshes the slots before editing.
    if (!wantsPresets || !presetsAvailable || owner.connection !== "open") {
      readingPresets = false;
      return;
    }
    const request = new AbortController();
    readingPresets = true;
    presetError = undefined;
    void owner
      .request<ThreadPresetsListResponse>(
        "thread/presets/list",
        { threadId },
        { signal: request.signal },
      )
      .then(({ presets }) => {
        if (!request.signal.aborted)
          presetCatalog = { machine: owner, thread: threadId, slots: presets };
      })
      .catch((failure) => {
        if (!request.signal.aborted) presetError = errorText(failure);
      })
      .finally(() => {
        if (!request.signal.aborted) readingPresets = false;
      });
    return () => request.abort();
  });

  function presetSaved(
    target: NonNullable<typeof presetDialog>,
    slot: number,
    preset: ComposerPreset | null,
  ) {
    if (target.machine !== machine || target.thread !== thread) return;
    const slots = { ...presetSlots };
    if (preset) slots[slot] = preset;
    else delete slots[slot];
    presetCatalog = { machine, thread, slots };
  }

  $effect(() => {
    const target = completionTarget;
    if (target?.kind !== "@" || !target.query || machine.connection !== "open") {
      fileChoices = undefined;
      return;
    }
    const { query } = target;
    const threadId = thread;
    const request = new AbortController();
    const timer = setTimeout(() => {
      void machine
        .request<ThreadFileSearchResponse>(
          "thread/files/search",
          { threadId, query },
          { signal: request.signal },
        )
        .then(({ paths }) => {
          if (!request.signal.aborted) fileChoices = { thread: threadId, query, paths };
        })
        .catch(() => {});
    }, 120);
    return () => {
      clearTimeout(timer);
      request.abort();
    };
  });

  const menuMatches = $derived.by((): CompletionChoice[] => {
    const target = completionTarget;
    if (!target || menuDismissed) return [];
    if (target.kind === "@") {
      if (thread !== fileChoices?.thread || target.query !== fileChoices.query) return [];
      return fileChoices.paths.map((path) => ({
        name: path,
        description: "",
        insert: "",
        start: target.start,
        end: target.end,
        path,
      }));
    }
    const pool: Omit<CompletionChoice, "start" | "end">[] =
      target.kind === "/"
        ? SLASH_COMMANDS.map((c) => ({
            name: c.name,
            description: c.description,
            insert: c.name + ("argument" in c ? " " : ""),
          }))
        : skillCatalog.map((s) => ({
            name: "$" + s.name,
            display: s.display,
            description: s.description,
            insert: "$" + s.name + " ",
            skill: { name: s.name, path: s.path },
          }));
    const q = target.query;
    const matched = !q
      ? pool
      : [
          ...pool.filter((c) => c.name.slice(1).toLowerCase().startsWith(q)),
          ...pool.filter(
            (c) =>
              !c.name.slice(1).toLowerCase().startsWith(q) &&
              (c.name.slice(1).toLowerCase().includes(q) ||
                c.display?.toLowerCase().includes(q) ||
                c.description.toLowerCase().includes(q)),
          ),
        ];
    if (target.kind === "$" && presetsAvailable && presetSlots) {
      for (const [slot, preset] of Object.entries(presetSlots)) {
        if (!preset) continue;
        const label = preset.label.trim().split("\n", 1)[0];
        if (label && label.toLowerCase().includes(q)) {
          matched.push({
            name: label,
            description: `Preset ${slot}: ${preset.input.trim().split("\n", 1)[0]}`,
            insert: preset.input,
            preset: slot,
          });
        }
      }
    }
    return matched.map((c) => ({ ...c, start: target.start, end: target.end }));
  });

  $effect(() => {
    void completionTarget?.kind;
    void completionTarget?.query;
    menuDismissed = false;
    menuIndex = 0;
  });

  async function pickCompletion(choice: CompletionChoice) {
    if (choice.preset !== undefined && (!presetsAvailable || disabled)) return;
    const draft = text;
    const position = caret;
    const before = text.slice(0, choice.start);
    const after = text.slice(choice.end);
    let insert = choice.insert;
    if (choice.path) {
      if (disabled) return;
      fileRequest?.abort();
      const pending = new AbortController();
      const target = { machine, thread };
      fileRequest = pending;
      error = undefined;
      let item: PendingAttachment;
      try {
        item = await hostPathAttachment(target, choice.path, newIdentity(), pending.signal);
      } catch (failure) {
        if (!pending.signal.aborted)
          error = failure instanceof Error ? failure.message : "Could not check that path.";
        return;
      } finally {
        if (fileRequest === pending) fileRequest = undefined;
      }
      if (
        pending.signal.aborted ||
        target.machine !== machine ||
        target.thread !== thread ||
        draft !== text ||
        position !== caret
      )
        return;
      const placeholder = labelAttachment(item, before + after);
      insert = placeholder + (/^[ \t]/.test(after) ? "" : " ");
      items = [...items, { ...item, placeholder }];
    }
    const replacement = before + insert + after;
    if (choice.skill) {
      editor.skills = [
        ...skillsAfterEdit(editor.text, replacement, editor.skills),
        { ...choice.skill, start: choice.start },
      ];
      editor.text = replacement;
    }
    text = replacement;
    const nextCaret = choice.start + insert.length;
    caret = nextCaret;
    menuDismissed = true;
    void tick().then(() => {
      if (!field) return;
      field.focus();
      field.setSelectionRange(nextCaret, nextCaret);
    });
  }

  export function addReference(value: string, description: string, source: string) {
    if (disabled || shell) return false;
    const labels = new Set(items.map(referenceLabel));
    let placeholder = `[Selected ${description}]`;
    for (let number = 2; labels.has(placeholder) || text.includes(placeholder); number++) {
      placeholder = `[Selected ${description} ${number}]`;
    }
    let at = field?.selectionStart() ?? text.length;
    for (const { start, end } of attachmentRanges(text, items)) {
      if (start < at && at < end) at = end;
    }
    const before = text.slice(0, at);
    const after = text.slice(at);
    const insert = (before && !/\s$/.test(before) ? " " : "") + placeholder + " ";
    text = before + insert + after;
    items = [
      ...items,
      {
        id: newIdentity(),
        file: new File([value], description),
        reference: source,
        placeholder,
      },
    ];
    caret = at + insert.length;
    void tick().then(() => {
      field?.focus();
      field?.setSelectionRange(at + insert.length, at + insert.length);
    });
    return true;
  }

  async function send(
    queue = false,
    prepared?: Attachment[],
    replace = false,
    onStarted?: InitialInput["onstarted"],
  ) {
    if (controller || fileRequest || tray?.checkingPath()) return;
    let bodyText = text;
    const itemCount = prepared?.length ?? activeItems.length;
    if (!bodyText.trim() && !itemCount) return;
    const pending = new AbortController();
    controller = pending;
    sending = true;
    progress = "Sending…";
    error = undefined;
    const origin = editor;
    let draftItems = [...items];
    let openingDialog = false;
    const finishInDialog = () => {
      openingDialog = true;
      sending = false;
      progress = "";
    };
    const clear = () => {
      origin.text = "";
      origin.items = [];
      origin.skills = [];
      if (editor === origin) {
        text = "";
        items = [];
      }
    };
    const readAttachments = async () => {
      const attachments =
        prepared ??
        (await prepareAttachments(
          submissionAttachments(bodyText, draftItems),
          pending.signal,
          (read, total) => {
            progress = total
              ? `Reading attachments: ${Math.round((read / total) * 100)}%`
              : "Preparing input…";
          },
        ));
      pending.signal.throwIfAborted();
      if (editor !== origin) throw new DOMException("Draft changed", "AbortError");
      progress = "Sending…";
      return attachments;
    };
    try {
      if (!prepared && draftItems.some((item) => "path" in item && item.deferred)) {
        progress = "Checking attachments…";
        const resolved = await resolveDeferredPathAttachments(
          { machine, thread },
          bodyText,
          draftItems,
          pending.signal,
        );
        pending.signal.throwIfAborted();
        if (editor !== origin) return;
        origin.text = resolved.text;
        origin.items = [...resolved.items];
        text = resolved.text;
        items = [...resolved.items];
        if (!resolved.ok) {
          error = resolved.error;
          return;
        }
        bodyText = resolved.text;
        draftItems = [...resolved.items];
        progress = "Sending…";
      }
      let recovered = false;
      const result = await semantic.sendInput(
        bodyText,
        { count: itemCount, read: readAttachments },
        {
          queue,
          signal: pending.signal,
          skills: skillsAfterEdit(origin.text, bodyText, origin.skills),
          slashActions: {
            archive() {
              finishInDialog();
              openArchive();
              text = "";
            },
            rewind() {
              if (stoppable) return "Wait for the thread to be idle before rewinding.";
              finishInDialog();
              rewindThread = thread;
              text = "";
            },
            async oneOff(task, read, skills) {
              if (oneOffOpen && !replace) {
                finishInDialog();
                replacing = true;
                return;
              }
              return ononeoff({ text: task, items: draftItems, attachments: await read(), skills });
            },
          },
          onStarted,
          onRecovery: () => {
            recovered = true;
            clear();
          },
        },
      );
      if (result) {
        if (!recovered && editor === origin) error = result;
        return;
      }
      if (openingDialog) return;
      clear();
      await tick();
      if (editor === origin) field?.focus();
    } catch (failure) {
      if (!pending.signal.aborted && editor === origin) {
        error = failure instanceof Error ? failure.message : "Could not send message.";
      }
    } finally {
      sending = false;
      controller = undefined;
      progress = "";
    }
  }

  async function stop() {
    error = await semantic.interrupt();
  }

  export function focus() {
    field?.focus();
  }

  export function openArchive() {
    archiveTarget = { thread, pending: false };
    error = undefined;
  }

  async function confirmArchive() {
    const target = archiveTarget;
    if (!target || target.pending || target.thread !== thread) return;
    target.pending = true;
    target.error = undefined;
    const failure = await onarchive();
    if (archiveTarget !== target) return;
    if (failure) {
      target.error = failure;
      target.pending = false;
    } else archiveTarget = undefined;
  }

  async function rewrite(next: string) {
    text = next;
    caret = next.length;
    error = undefined;
    await tick();
    field?.focus();
    field?.setSelectionRange(next.length, next.length);
  }

  function acceptPrediction() {
    if (prediction) void rewrite(prediction);
  }

  function recordSelection(start: number, end: number) {
    caret = start;
    selectedText = start !== end;
  }

  function onkeydown(event: KeyboardEvent) {
    if (event.isComposing || (event.key === "Enter" && event.keyCode === 229)) return;
    if (fileRequest && event.key === "Escape") {
      fileRequest.abort();
      fileRequest = undefined;
    }
    if (menuMatches.length) {
      if (event.key === "ArrowDown") {
        event.preventDefault();
        menuIndex = (menuIndex + 1) % menuMatches.length;
        return;
      }
      if (event.key === "ArrowUp") {
        event.preventDefault();
        menuIndex = (menuIndex - 1 + menuMatches.length) % menuMatches.length;
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        menuDismissed = true;
        return;
      }
      if (event.key === "Tab") {
        event.preventDefault();
        pickCompletion(menuMatches[menuIndex] ?? menuMatches[0]);
        return;
      }
      if (event.key === "Enter" && !event.shiftKey && !touch) {
        const chosen = menuMatches[menuIndex] ?? menuMatches[0];
        if (
          chosen &&
          (completionTarget?.kind !== "/" || text.trim().toLowerCase() !== chosen.name)
        ) {
          event.preventDefault();
          pickCompletion(chosen);
          return;
        }
      }
    }
    if (
      prediction &&
      (event.key === "Tab" || event.key === "ArrowRight") &&
      !event.shiftKey &&
      !event.ctrlKey &&
      !event.metaKey &&
      !event.altKey
    ) {
      event.preventDefault();
      acceptPrediction();
      return;
    }
    if (event.key === "Enter" && !event.shiftKey && !touch) {
      event.preventDefault();
      if (canSend) void send();
    } else if (event.key === "Tab" && !event.shiftKey && (shell || stoppable) && canSend) {
      event.preventDefault();
      void send(true);
    }
  }
</script>

<div class="mx-auto w-full max-w-column px-4 pt-2 pb-4 max-sm:px-2 max-sm:pb-safe-2">
  {#if error}<Alert class="mb-2">{error}</Alert>{/if}
  {#if menuMatches.length}
    <div
      role="listbox"
      aria-label={completionTarget?.kind === "@"
        ? "Files"
        : completionTarget?.kind === "$"
          ? presetsAvailable
            ? "Skills and presets"
            : "Skills"
          : "Slash commands"}
      class="mb-1.5 max-h-56 overflow-y-auto rounded-card border border-border bg-surface p-1 shadow-sm"
    >
      {#each menuMatches as cmd, i (cmd.preset !== undefined ? `preset:${cmd.preset}` : (cmd.skill?.path ?? cmd.name))}
        <button
          type="button"
          role="option"
          aria-selected={i === menuIndex}
          class={[
            "flex w-full items-baseline justify-between gap-3 rounded-control px-2.5 py-1.5 text-left text-small",
            i === menuIndex ? "bg-active text-ink" : "text-ink hover:bg-hover",
          ]}
          onmousedown={(e) => {
            e.preventDefault();
          }}
          onclick={() => pickCompletion(cmd)}
        >
          <span class="min-w-0 font-mono font-medium"
            >{#if cmd.path}<Path path={cmd.path} />{:else}{cmd.display ?? cmd.name}{/if}</span
          >
          <span class="truncate text-muted">{cmd.description}</span>
        </button>
      {/each}
    </div>
  {/if}
  <ComposerCard
    accepts={!disabled && !shell}
    onfiles={(files) => tray?.add(files)}
    onsubmit={() => void send()}
  >
    <Attachments
      bind:this={tray}
      bind:items
      bind:text
      bind:pathing
      machine={machine.label}
      previewTarget={{ machine, thread }}
      disabled={disabled || shell}
    />
    <div class="flex items-start touch:[&_button]:min-h-11 touch:[&_button]:min-w-11">
      <div class="relative min-w-0 flex-1">
        {#if shell && !bareShell}
          <pre
            class={[
              composerText,
              shellText,
              "pointer-events-none absolute inset-0 min-h-14 overflow-hidden pt-3 pb-1.5 forced-colors:hidden",
              selectedText && "invisible",
            ]}
            aria-hidden="true"
            bind:this={syntax}><Code
              code={text.endsWith("\n") ? text + " " : text}
              lang="bash"
            /></pre>
        {/if}
        {#if bareShell}
          <span
            id="composer-shell-target"
            class="pointer-events-none absolute inset-x-0 top-3 truncate px-4 font-mono leading-[22px] text-muted max-sm:px-3.5 max-sm:text-base"
            ><span class="invisible" aria-hidden="true">! </span>Shell command on {machine.label}</span
          >
        {:else if prediction}
          <span
            id="composer-prediction"
            class="pointer-events-none absolute inset-x-0 top-3 truncate px-4 leading-[22px] text-muted max-sm:px-3.5 max-sm:text-base"
            >{prediction}</span
          >
        {/if}
        <DraftField
          id="composer-text"
          label={shell ? "Shell command" : `Message ${addressee}`}
          bind:this={field}
          class={[
            composerText,
            "relative min-h-14 pt-3 pb-1.5",
            shell && shellText,
            highlightShell && "shell-highlighted",
            shell && "overflow-x-hidden overflow-y-auto",
          ]}
          plain={shell}
          style="caret-color: var(--text)"
          describedby={bareShell
            ? "composer-shell-target"
            : prediction
              ? "composer-prediction"
              : undefined}
          keyshortcuts={prediction ? "Tab ArrowRight" : undefined}
          onscroll={(target) => {
            if (syntax) {
              syntax.scrollTop = target.scrollTop;
              syntax.scrollLeft = target.scrollLeft;
            }
          }}
          bind:value={text}
          {items}
          onselection={recordSelection}
          {onkeydown}
          onpaste={(event) => {
            const files = event.clipboardData?.files;
            if (!files?.length || disabled || shell) return;
            if (!event.clipboardData?.types.includes("text/plain")) event.preventDefault();
            tray?.add(files);
          }}
          {disabled}
          placeholder={shell || prediction
            ? ""
            : asking
              ? "Answer the prompt above, or write a follow-up…"
              : stoppable
                ? "Steer the active turn…"
                : `Message ${addressee}`}
        />
      </div>
      {#if prediction}
        <Button
          variant="quiet"
          icon="check"
          label={prediction}
          title="Insert suggestion as an editable draft"
          class="mt-1 mr-2"
          onclick={acceptPrediction}
        >
          {#if !touch}<kbd class={kbd} aria-hidden="true">Tab</kbd>{/if}
        </Button>
      {/if}
    </div>
    <InputGroup.Addon
      align="block-end"
      class="{composerTools} cursor-default gap-1 text-body font-normal select-auto touch:[&_button]:min-h-11 touch:[&_button]:min-w-11"
    >
      <div class="flex min-w-0 flex-1 flex-wrap items-center gap-1">
        <Menu label="Attach" icon="plus" disabled={disabled || shell}>
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
        <Button
          variant="quiet"
          icon="terminal"
          label="Shell command"
          title={`Run a shell command on ${machine.label} (!)`}
          aria-pressed={shell}
          disabled={disabled || !!activeItems.length}
          onclick={() => void rewrite(shell ? text.slice(1) : "!" + text)}
        />
        {#if shell}<Tag mono>Shell</Tag>{/if}
        {#if presetsAvailable}
          <Button
            variant="quiet"
            icon="presets"
            label="Presets"
            aria-haspopup="dialog"
            {disabled}
            onclick={() =>
              (presetDialog = { machine, thread, seed: activeItems.length || shell ? "" : text })}
          />
        {/if}
        <ThreadSettings {disabled} onerror={(message) => (settingsError = message)} />
        {#if role}<Tag color={stream.roleColor(role)} class="ml-1.5 max-sm:hidden">{role}</Tag>{/if}
        {#if progress}<span class="min-w-0 truncate text-small text-muted">{progress}</span>{/if}
      </div>
      {#if stoppable}
        <Button
          variant="quiet"
          icon="stop"
          label="Interrupt turn"
          class="self-end"
          disabled={semantic.connection !== "open"}
          onclick={() => void stop()}
        />
      {/if}
      <Button
        type="submit"
        variant="primary"
        shape="round"
        icon="send"
        label={shell ? "Run shell command" : stoppable ? "Steer turn" : "Send message"}
        class="self-end"
        disabled={!canSend}
      />
    </InputGroup.Addon>
  </ComposerCard>
  {#if settingsError}<Alert class="mt-2 wrap-anywhere">{settingsError}</Alert>{/if}
  {#if unavailable}
    <p class="mt-1.5 px-1 text-small text-muted">{unavailable}</p>
  {:else if stream.notice}
    <p class="mt-1.5 px-1 text-small text-muted" role="status">{stream.notice}</p>
  {/if}
</div>

{#if rewindThread === thread}
  <Rewind {machine} {thread} busy={stoppable} onclose={() => (rewindThread = undefined)} />
{/if}

{#if presetOpen && presetDialog}
  {@const target = presetDialog}
  <ComposerPresets
    {machine}
    {thread}
    seed={target.seed}
    slots={presetSlots}
    loading={readingPresets}
    readError={presetError}
    {disabled}
    onclose={() => (presetDialog = undefined)}
    onsaved={(slot, preset) => presetSaved(target, slot, preset)}
    onrecall={(draft) => {
      presetDialog = undefined;
      items = [];
      editor.skills = [];
      void rewrite(draft);
    }}
  />
{/if}

{#if archiveTarget?.thread === thread}
  <Dialog title="Archive thread?" onclose={() => (archiveTarget = undefined)}>
    <p>
      {stoppable
        ? "This will stop the current turn and archive the thread."
        : "This will archive the current thread."}
    </p>
    {#if archiveTarget.error}<Alert class="mt-2">{archiveTarget.error}</Alert>{/if}
    <div class="mt-4 flex justify-end gap-2">
      <Button variant="quiet" data-initial-focus onclick={() => (archiveTarget = undefined)}
        >{archiveTarget.pending ? "Dismiss" : "Cancel"}</Button
      >
      <Button variant="danger" disabled={archiveTarget.pending} onclick={confirmArchive}
        >{archiveTarget.pending ? "Archiving…" : "Archive"}</Button
      >
    </div>
  </Dialog>
{/if}

{#if replacing}
  <Dialog title="Replace One-off?" onclose={() => (replacing = false)}>
    <p>Replace closes One-off, stops its running work and starts from a fresh snapshot of Main.</p>
    <div class="mt-4 flex justify-end gap-2">
      <Button variant="quiet" data-initial-focus onclick={() => (replacing = false)}>Cancel</Button>
      <Button
        variant="danger"
        onclick={() => {
          replacing = false;
          void send(false, undefined, true);
        }}>Replace</Button
      >
    </div>
  </Dialog>
{/if}

<style>
  .relative > :global(.shell-highlighted) {
    color: transparent;
  }

  @media (forced-colors: active) {
    .relative > :global(.shell-highlighted) {
      color: CanvasText;
    }
  }
</style>
