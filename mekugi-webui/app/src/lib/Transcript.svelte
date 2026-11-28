<script lang="ts">
  import { tick, untrack } from "svelte";
  import { preferredScrollBehavior } from "./diff";
  import EntryView from "./EntryView.svelte";
  import { date } from "./format";
  import {
    editEntryPaths,
    failed,
    hasDetails,
    isEditEntry,
    isUser,
    isView,
    responseText,
  } from "./kinds";
  import Path from "./Path.svelte";
  import { preferences } from "./preferences.svelte";
  import ReferenceSelection from "./ReferenceSelection.svelte";
  import ResponseCopy from "./ResponseCopy.svelte";
  import type { Entry } from "./protocol";
  import type { Stream } from "./stream.svelte";
  import Icon from "./ui/Icon.svelte";
  import { Skeleton } from "./ui/skeleton";
  import { emptyBox, revealed, rowAction } from "./ui/styles";
  import { segmentWorkdirRows, workdirGroups } from "./workdir";

  let {
    entries,
    transcript,
    owner = "/root",
    workspace,
    jumps = 0,
    empty = "No messages yet.",
    onselect,
    onreference,
    onchanges,
    hasSavedChange,
    onhiddenhooks,
  }: {
    entries: readonly Entry[];
    transcript?: Stream;
    owner?: string;
    workspace?: string;
    jumps?: number;
    empty?: string;
    onselect?: (entry: Entry) => void;
    onreference?: (text: string, description: string, source: string) => boolean;
    onchanges?: (paths: string[]) => void;
    hasSavedChange?: (paths: string[]) => boolean;
    onhiddenhooks?: (count: number) => void;
  } = $props();

  let scroller = $state<HTMLElement>();
  let contentEl = $state<HTMLElement>();
  let pinned = $state(true);
  let mountedSource: Stream | undefined;
  let mountedScroller: HTMLElement | undefined;
  let pagingViewport: HTMLElement | undefined;

  type ReadingPosition = NonNullable<Stream["scroll"]>;
  const rowIdentity = (entry: Entry) =>
    entry.rowKey ?? JSON.stringify([entry.turnId, entry.itemId ?? entry.seq]);

  const isHiddenHook = (entry: Entry): boolean => {
    if (entry.kind !== "hook") return false;
    if (preferences.hooks === "all") return false;
    if (preferences.hooks === "off") return true;
    return !entry.values?.some(failed);
  };

  const filtered = $derived.by(() => {
    let hiddenHooks = 0;
    const grouped: {
      entry: Entry;
      rawAt: string;
      turn?: number;
      views?: Entry[];
      thoughts?: Entry[];
    }[] = [];
    for (const entry of entries) {
      if (isHiddenHook(entry)) {
        hiddenHooks++;
        continue;
      }
      const last = grouped.at(-1);
      if (last?.thoughts && entry.kind === "reasoning" && entry.agent === last.entry.agent) {
        last.thoughts.push(entry);
        continue;
      }
      if (
        last?.views &&
        isView(entry) &&
        entry.agent === last.entry.agent &&
        entry.values?.[0]?.verb === last.entry.values?.[0]?.verb
      ) {
        last.views.push(entry);
        continue;
      }
      const rawAt = entry.observed ? date(entry.observed).slice(11) : "";
      grouped.push({
        entry,
        rawAt,
        turn: entry.turn,
        views: isView(entry) ? [entry] : undefined,
        thoughts: entry.kind === "reasoning" ? [entry] : undefined,
      });
    }
    let previous = "";
    const out: {
      entry: Entry;
      id: string;
      at: string;
      turn?: number;
      views?: Entry[];
      thoughts?: Entry[];
    }[] = [];
    for (const row of grouped) {
      let rawAt = row.rawAt;
      let thoughts = row.thoughts;
      if (thoughts && preferences.thoughts === "hidden") {
        thoughts = thoughts.filter((entry) => entry.running);
        const live = thoughts[0];
        if (!live) continue;
        rawAt = live.observed ? date(live.observed).slice(11) : "";
      }
      const user = isUser(row.entry);
      out.push({
        entry: row.entry,
        id: rowIdentity(row.entry),
        at: user || rawAt !== previous ? rawAt : "",
        turn: row.turn,
        views: row.views,
        thoughts,
      });
      previous = rawAt;
    }
    return { rows: out, hiddenHooks };
  });
  const rows = $derived(filtered.rows);
  // Latest rows are already visible at the bottom on initial open.
  const eager = $derived(new Set(rows.slice(-12)));
  $effect(() => {
    onhiddenhooks?.(filtered.hiddenHooks);
  });

  const segments = $derived(segmentWorkdirRows(rows, workdirGroups(rows, workspace)));
  const segmentKey = (segment: (typeof segments)[number]) =>
    segment.kind === "row"
      ? segment.row.id
      : `workdir:${JSON.stringify([segment.workdir, segment.rows[0].id])}`;

  function currentEntry(entry: Entry): Entry {
    return transcript?.entry(entry.seq) ?? entry;
  }

  function renderedThoughts(thoughts: Entry[] | undefined): Entry[] | undefined {
    if (!thoughts) return undefined;
    const current = thoughts.map(currentEntry);
    if (preferences.thoughts !== "hidden") return current;
    return current.map((entry) => ({
      ...entry,
      values: entry.values?.filter((value) => value.live),
    }));
  }

  function readingPosition(viewport: HTMLElement): ReadingPosition {
    const position: ReadingPosition = { top: viewport.scrollTop, pinned };
    if (pinned) return position;
    const { left, top, width, height } = viewport.getBoundingClientRect();
    const row = viewport.ownerDocument
      .elementFromPoint(left + width / 2, top + Math.min(48, height / 2))
      ?.closest<HTMLElement>("[data-entry-id]");
    if (row?.dataset.entryId && viewport.contains(row)) {
      position.anchor = { id: row.dataset.entryId, offset: row.getBoundingClientRect().top - top };
    }
    return position;
  }

  function findRow(viewport: HTMLElement, id: string) {
    return viewport.querySelector<HTMLElement>(`[data-entry-id="${CSS.escape(id)}"]`);
  }

  function restorePosition(viewport: HTMLElement, position: ReadingPosition) {
    if (position.pinned) {
      viewport.scrollTop = viewport.scrollHeight;
      return;
    }
    const anchor = position.anchor;
    const row = anchor && findRow(viewport, anchor.id);
    if (row && row.classList.contains("deferred") && -anchor.offset >= row.offsetHeight) {
      // A deep offset may land past the skipped row's placeholder. Keep only
      // this row laid out until the restored position has been painted.
      row.setAttribute("data-anchor-layout", "");
      requestAnimationFrame(() => {
        requestAnimationFrame(() => row.removeAttribute("data-anchor-layout"));
      });
    }
    // Align an existing row instead of measuring the list height: retained
    // rows can grow at the other end, and skipped rows have estimated heights.
    const top = row
      ? viewport.scrollTop +
        row.getBoundingClientRect().top -
        viewport.getBoundingClientRect().top -
        anchor.offset
      : position.top;
    if (Math.abs(viewport.scrollTop - top) >= 1) viewport.scrollTop = top;
  }

  async function maybeEarlier(viewport: HTMLElement) {
    const source = transcript;
    if (
      !source?.hasEarlier ||
      source.loadingEarlier ||
      source.hydrating ||
      pagingViewport === viewport ||
      viewport.scrollTop >= 120
    ) {
      return;
    }
    pagingViewport = viewport;
    try {
      do {
        if (!(await source.earlier())) return;
        await tick();
        if (scroller !== viewport || transcript !== source) return;
        // A batch fully hidden by preferences may not trigger another resize.
      } while (source.hasEarlier && viewport.scrollHeight <= viewport.clientHeight);
    } finally {
      if (pagingViewport === viewport) pagingViewport = undefined;
    }
  }

  function onscroll(event: Event & { currentTarget: HTMLElement }) {
    const viewport = event.currentTarget;
    if (!transcript || mountedScroller !== viewport || mountedSource !== transcript) return;
    // Opening the phone/tablet drawer can reflow rows, but its inert canvas
    // is excluded from hit testing. Keep the last reading anchor while covered.
    if (viewport.closest("[inert]")) return;
    const distance = viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight;
    pinned = distance < 48;
    transcript.scroll = readingPosition(viewport);
    void maybeEarlier(viewport);
  }

  $effect(() => {
    const source = transcript;
    const viewport = scroller;
    if (!viewport) return;
    mountedSource = source;
    mountedScroller = viewport;
    const position = source?.scroll ?? { top: 0, pinned: true };
    pinned = position.pinned;
    restorePosition(viewport, position);
  });

  const focusables = "a[href], button, input, select, summary, textarea, [tabindex]";
  $effect.pre(() => {
    // Bulk rows change on structural events, not when text is appended.
    void segments;
    const source = transcript;
    const viewport = scroller;
    if (!viewport || mountedSource !== source || mountedScroller !== viewport) return;

    untrack(() => {
      const position = pinned ? undefined : readingPosition(viewport);
      const doc = viewport.ownerDocument;
      const control = doc.activeElement;
      const row = control?.closest<HTMLElement>("[data-entry-id]");
      const id = row?.dataset.entryId;
      const index =
        control && row && viewport.contains(row)
          ? [...row.querySelectorAll(focusables)].indexOf(control)
          : -1;
      if (!position && index < 0) return;

      void tick().then(() => {
        if (transcript !== source || scroller !== viewport) return;
        if (position && !pinned) restorePosition(viewport, position);
        if (!id || index < 0 || doc.activeElement !== doc.body) return;
        const replacement = findRow(viewport, id)?.querySelectorAll<HTMLElement>(focusables)[index];
        if (
          replacement &&
          replacement.tagName === control?.tagName &&
          replacement.getAttribute("aria-label") === control.getAttribute("aria-label")
        ) {
          replacement.focus({ preventScroll: true });
        }
      });
    });
  });

  $effect(() => {
    void segments;
    void jumps;
    const viewport = scroller;
    if (!pinned || !viewport) return;
    void tick().then(() => {
      if (pinned && scroller === viewport) viewport.scrollTop = viewport.scrollHeight;
    });
  });

  $effect(() => {
    const viewport = scroller;
    const content = contentEl;
    if (!viewport || !content || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      if (scroller !== viewport) return;
      if (pinned) viewport.scrollTop = viewport.scrollHeight;
      // A short initial page may not be scrollable at all.
      if (viewport.scrollHeight <= viewport.clientHeight) void maybeEarlier(viewport);
    });
    observer.observe(content);
    observer.observe(viewport);
    return () => observer.disconnect();
  });

  function jumpToLatest() {
    pinned = true;
    scroller?.scrollTo({ top: scroller.scrollHeight, behavior: preferredScrollBehavior() });
  }
</script>

{#snippet transcriptRow(row: (typeof rows)[number], groupWorkdir?: string)}
  {@const views = row.views?.map(currentEntry)}
  {@const thoughts = renderedThoughts(row.thoughts)}
  {@const entry = thoughts?.[0] ?? currentEntry(row.entry)}
  {@const editPaths = isEditEntry(entry) ? editEntryPaths(entry) : []}
  {@const openChanges =
    !!onchanges &&
    isEditEntry(entry) &&
    (!hasSavedChange || (editPaths.length > 0 && hasSavedChange(editPaths)))}
  {@const details = !!onselect && (!!entry.content?.itemId || hasDetails(entry, owner))}
  {@const hasRowAction = openChanges || details}
  {@const response =
    !isUser(entry) && !entry.assignment
      ? responseText(entry.kind, entry.text, entry.values?.[0]?.body)
      : ""}
  <div
    class="group/row relative"
    class:deferred={!eager.has(row)}
    data-entry={entry.seq}
    data-entry-id={row.id}
  >
    <EntryView
      {entry}
      at={row.at}
      stamp="Time"
      timestampMode={preferences.timestamps}
      expandThoughts={preferences.thoughts === "expanded"}
      turn={row.turn}
      {views}
      {thoughts}
      {owner}
      {groupWorkdir}
      {workspace}
      ondetails={openChanges
        ? () => onchanges?.(editPaths)
        : details
          ? () => onselect?.(entry)
          : undefined}
    />
    {#if openChanges}
      <button
        type="button"
        class="{rowAction} {revealed} absolute top-1 -right-9 text-muted max-sm:-right-10"
        aria-label="View changes"
        title="View changes"
        onclick={() => onchanges?.(editPaths)}
      >
        <Icon name="diff" size={14} />
      </button>
    {:else if details}
      <button
        type="button"
        class="{rowAction} {revealed} absolute top-1 -right-9 text-muted max-sm:-right-10"
        aria-label="Details"
        title="Details"
        onclick={() => onselect?.(entry)}
      >
        <Icon name="expand" size={14} />
      </button>
    {/if}
    {#if response}
      <ResponseCopy text={response} gutter below={hasRowAction} />
    {/if}
  </div>
{/snippet}

<div class="relative min-h-0 flex-1">
  {#key transcript}
    <div
      bind:this={scroller}
      {onscroll}
      class="@container h-full overflow-y-auto overscroll-contain"
    >
      <div
        bind:this={contentEl}
        class="mx-auto max-w-[calc(var(--container-column)+184px)] pt-3 pr-23 pb-10 pl-4 @max-[980px]:pr-11 max-sm:pt-2 max-sm:pb-6"
      >
        {#if transcript?.loadingEarlier}
          <span role="status" class="sr-only">Loading earlier turns</span>
        {/if}
        {#if segments.length}
          {#each segments as seg (segmentKey(seg))}
            {#if seg.kind === "group"}
              <section data-workdir-group={seg.workdir} class="my-2 first:mt-0">
                <div
                  data-workdir-heading={seg.workdir}
                  class="sticky top-0 z-10 grid max-w-[920px] grid-cols-[60px_minmax(0,1fr)] gap-x-4 bg-surface/95 py-1.5 backdrop-blur-sm max-sm:grid-cols-1"
                >
                  <span class="max-sm:hidden" aria-hidden="true"></span>
                  <div
                    class="flex min-w-0 items-baseline gap-1.5 border-b border-rule/80 pb-1 text-small"
                  >
                    <span class="shrink-0 text-muted">In</span>
                    <span class="min-w-0 font-mono text-ink"><Path path={seg.workdir} /></span>
                  </div>
                </div>
                {#each seg.rows as row (row.id)}
                  {@render transcriptRow(row, seg.workdir)}
                {/each}
              </section>
            {:else}
              {@render transcriptRow(seg.row)}
            {/if}
          {/each}
        {:else if transcript?.hydrating || transcript?.connection === "connecting"}
          <div class="mt-6 ml-19 grid gap-3 max-sm:ml-0" role="status" aria-label="Loading thread">
            <Skeleton class="h-5 w-3/4 bg-active" />
            <Skeleton class="h-5 w-1/2 bg-active" />
            <Skeleton class="h-5 w-5/6 bg-active" />
          </div>
        {:else}
          <p class="{emptyBox} mt-6 ml-17 max-sm:ml-0">{empty}</p>
        {/if}
      </div>
    </div>
  {/key}
  {#if onreference}
    <ReferenceSelection root={scroller} {onreference} />
  {/if}
  {#if !pinned}
    <button
      type="button"
      class="absolute bottom-4 left-1/2 flex h-8 -translate-x-1/2 items-center gap-1.5 rounded-full border border-border bg-surface/95 px-3.5 text-small font-medium text-ink shadow-float backdrop-blur-sm transition-colors hover:border-control hover:bg-raised max-sm:h-10"
      onclick={jumpToLatest}
    >
      <Icon name="down" size={14} /> Latest
    </button>
  {/if}
</div>

<style>
  /* Keep the sticky heading and out-of-flow gutter controls outside containment. */
  .deferred > :global(article) {
    content-visibility: auto;
    contain-intrinsic-block-size: auto 4rem;
  }

  .deferred:focus-within > :global(article),
  .deferred:global([data-anchor-layout]) > :global(article) {
    content-visibility: visible;
  }
</style>
