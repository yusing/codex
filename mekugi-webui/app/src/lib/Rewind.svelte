<script lang="ts">
  import { onMount, tick } from "svelte";
  import type { Machine } from "./hub.svelte";
  import type { TurnsPage } from "./protocol";
  import { semantic, type RewindPreview } from "./semantic.svelte";
  import { itemToEntry } from "./stream.svelte";
  import { errorText } from "./transport";
  import Alert from "./ui/Alert.svelte";
  import Button from "./ui/Button.svelte";
  import Dialog from "./ui/Dialog.svelte";

  let {
    machine,
    thread,
    busy,
    onclose,
  }: {
    machine: Machine;
    thread: string;
    busy: boolean;
    onclose: () => void;
  } = $props();

  type Target = { turnId: string; itemId: string | null; label: string };
  let targets = $state<Target[]>([]);
  let chosen = $state<Target>();
  let preview = $state<RewindPreview>();
  let nextCursor = $state<string | null>(null);
  let loading = $state(false);
  let pending = $state(false);
  let failure = $state<string>();
  let actions = $state<HTMLElement>();
  const lifetime = new AbortController();
  let previewRequest: AbortController | undefined;

  onMount(() => {
    void load(null);
    return () => lifetime.abort();
  });

  async function load(cursor: string | null) {
    loading = true;
    failure = undefined;
    try {
      const page = await machine.request<TurnsPage>(
        "thread/turns/list",
        { threadId: thread, sortDirection: "desc", itemsView: "full", limit: 20, cursor },
        { signal: lifetime.signal },
      );
      for (const turn of page.data) {
        if (turn.status === "inProgress") continue;
        let first = true;
        for (const item of turn.items) {
          if (item.type !== "userMessage") continue;
          if (!first && !item.id) continue;
          const label = (itemToEntry(item, 0, 0, turn.id)?.text ?? "").replace(/\s+/g, " ").trim();
          targets.push({
            turnId: turn.id,
            itemId: first ? null : item.id,
            label: label || "User message (no text)",
          });
          first = false;
        }
      }
      nextCursor = page.nextCursor;
    } catch (err) {
      if (!lifetime.signal.aborted) failure = errorText(err, true);
    } finally {
      loading = false;
    }
  }

  async function choose(target: Target) {
    previewRequest = new AbortController();
    const signal = AbortSignal.any([lifetime.signal, previewRequest.signal]);
    chosen = target;
    failure = undefined;
    await tick();
    if (signal.aborted) return;
    actions?.querySelector<HTMLButtonElement>("[data-initial-focus]")?.focus();
    try {
      const result = await machine.request<RewindPreview>(
        "thread/revert/preview",
        { threadId: thread, beforeTurnId: target.turnId, beforeItemId: target.itemId },
        { signal },
      );
      if (!signal.aborted) preview = result;
    } catch (err) {
      if (!signal.aborted) preview = { changeIds: [], reason: errorText(err, true) };
    }
  }

  async function apply(changeIds: string[] | null) {
    if (!chosen || pending || busy) return;
    pending = true;
    failure = undefined;
    const err = await semantic.revert(machine, thread, chosen.turnId, chosen.itemId, changeIds);
    pending = false;
    if (err) failure = err;
    else onclose();
  }
</script>

<Dialog
  title={chosen ? "Rewind this thread?" : "Rewind before a user message"}
  onclose={() => {
    if (!pending) onclose();
  }}
>
  {#if chosen}
    <p class="break-words">{chosen.label}</p>
    <p class="mt-2 text-small text-muted">
      This removes the selected message and everything after it from this thread.
    </p>
    {#if !preview}
      <p class="mt-2 text-small text-muted" role="status">Checking captured edits…</p>
    {:else if preview.reason}
      <p class="mt-2 text-small text-muted">{preview.reason}</p>
    {/if}
    <div bind:this={actions} class="mt-4 flex flex-wrap justify-end gap-2">
      <Button
        variant="quiet"
        disabled={pending}
        onclick={() => {
          previewRequest?.abort();
          chosen = undefined;
          preview = undefined;
          failure = undefined;
        }}>Back</Button
      >
      <Button
        variant="quiet"
        data-initial-focus
        disabled={busy || pending}
        onclick={() => apply(null)}>Keep edits</Button
      >
      {#if preview?.changeIds.length}
        <Button
          variant="danger"
          disabled={busy || pending}
          onclick={() => apply(preview?.changeIds ?? null)}>Revert edits</Button
        >
      {/if}
    </div>
  {:else}
    <ul class="-mx-1 grid max-h-[min(26rem,50dvh)] gap-1 overflow-y-auto overscroll-contain p-1">
      {#each targets as target (`${target.turnId}:${target.itemId ?? ""}`)}
        <li>
          <Button
            variant="quiet"
            class="h-auto w-full justify-start wrap-anywhere whitespace-normal text-left"
            disabled={busy}
            onclick={() => choose(target)}>{target.label}</Button
          >
        </li>
      {/each}
    </ul>
    {#if loading}
      <p class="text-small text-muted" role="status">Loading user messages…</p>
    {:else if nextCursor}
      <Button variant="quiet" class="mt-2" onclick={() => load(nextCursor)}>Earlier messages</Button
      >
    {:else if !targets.length && !failure}
      <p class="text-small text-muted">No user messages available to rewind.</p>
    {/if}
  {/if}
  {#if busy}<p class="mt-2 text-small text-muted" role="status">
      Wait for the thread to be idle.
    </p>{/if}
  {#if failure}<Alert class="mt-2">{failure}</Alert>{/if}
</Dialog>
