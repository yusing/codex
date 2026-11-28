<script lang="ts">
  import Path from "./Path.svelte";
  import { untrack } from "svelte";
  import { workspacePath } from "./diff";
  import { followBottom } from "./follow-bottom";
  import DiffView from "./DiffView.svelte";
  import AgentName from "./AgentName.svelte";
  import type { Preview } from "./protocol";
  import Button from "./ui/Button.svelte";
  import Dot from "./ui/Dot.svelte";
  import Tag from "./ui/Tag.svelte";

  // Edits appear alongside the transcript. Each completed call stays for its
  // own settling interval. Calls already complete when the view opens are
  // history, not arrivals.
  let {
    previews,
    owner,
    onsaved,
  }: { previews: Preview[]; owner: string; onsaved: (path: string) => void } = $props();

  // The terminal's completion hold.
  const hold = 1500;
  interface Card {
    preview: Preview;
    until?: number;
  }
  let cards = $state.raw<Card[]>([]);
  // Calls already shown or passed over; the server keeps completed calls
  // until their turn ends, and they must not settle again.
  const seen = new Set<string>();
  let primed = false;
  $effect(() => {
    const withdrawn = new Set(previews.filter((p) => p.Withdrawn).map((p) => p.ID));
    const mine = previews.filter((p) => !p.Withdrawn && p.Files.length > 0);
    untrack(() => {
      const now = Date.now();
      const current = new Map(mine.map((p) => [p.ID, p]));
      const next: Card[] = [];
      for (const card of cards) {
        if (withdrawn.has(card.preview.ID)) continue;
        const preview = current.get(card.preview.ID);
        current.delete(card.preview.ID);
        // A withdrawn call settles from its last content; a repeated
        // completion cannot restart its hold.
        const until = preview && !preview.Complete ? undefined : (card.until ?? now + hold);
        next.push({ preview: preview ?? card.preview, until });
      }
      for (const preview of current.values())
        if (!preview.Complete || (primed && !seen.has(preview.ID)))
          next.push({ preview, until: preview.Complete ? now + hold : undefined });
      seen.clear();
      for (const preview of mine) seen.add(preview.ID);
      primed = true;
      cards = next.filter((card) => card.until === undefined || card.until > now);
    });
  });
  $effect(() => {
    const ends = cards.flatMap((card) => (card.until ? [card.until] : []));
    if (!ends.length) return;
    const timer = setTimeout(
      () => (cards = cards.filter((card) => !card.until || card.until > Date.now())),
      Math.max(0, Math.min(...ends) - Date.now()) + 20,
    );
    return () => clearTimeout(timer);
  });

  // Hiding leaves only the transcript until another call arrives.
  let hidden = $state<string[]>([]);
  const shown = $derived(cards.filter((card) => !hidden.includes(card.preview.ID)));
  const writing = $derived(shown.some((card) => !card.until && !card.preview.Settling));
  const hasSaved = $derived(shown.some(({ preview }) => preview.SavedPath));
</script>

{#if shown.length}
  <section
    class="@container flex min-h-0 min-w-0 flex-1 flex-col border-t border-rule @min-[40rem]/thread-panes:border-t-0 @min-[40rem]/thread-panes:border-l"
    aria-label="Live edits"
  >
    <div
      class="mx-auto flex w-[calc(100%-32px)] flex-none items-center gap-2 border-b border-rule pt-3 pb-2.5"
    >
      <h2 class="flex items-center gap-2 font-semibold">
        <Dot tone={writing ? "running" : "done"} size={6} />Live
      </h2>
      <span class="text-small text-muted">
        {writing ? "Writing" : hasSaved ? "Captured" : "Settling"}
      </span>
      <Button
        size="sm"
        variant="quiet"
        class="ml-auto"
        onclick={() => (hidden = cards.map((card) => card.preview.ID))}>Hide Live Diff</Button
      >
    </div>
    <div class="min-h-0 flex-1 overflow-hidden">
      <div
        class="mx-auto grid h-full w-[calc(100%-32px)] auto-rows-fr grid-cols-1 gap-3 pb-6 @min-[40rem]:not-has-[>article:only-child]:grid-cols-2"
      >
        {#each shown as { preview, until } (preview.ID)}
          <article class="flex min-h-0 min-w-0 flex-col overflow-hidden pt-2 pb-2.5 wrap-anywhere">
            <div class="flex flex-none flex-wrap items-center gap-x-2 gap-y-1 text-small">
              {#if preview.Caller === "User shell"}
                <span class="text-muted">User shell</span>
              {:else}
                <AgentName name={preview.Caller} current={owner} />
              {/if}
              {#if preview.Tool}<Tag mono>{preview.Tool}</Tag>{/if}
              {#if preview.Status}<span class="text-muted">{preview.Status}</span>{/if}
              <span
                class={[
                  "ml-auto inline-flex items-center gap-1.5",
                  until || preview.Settling ? "text-muted" : "text-accent",
                ]}
                >{#if !until && !preview.Settling}<Dot tone="running" size={6} />{/if}{until
                  ? "done"
                  : preview.Settling
                    ? "settling"
                    : "writing"}</span
              >
              {#if preview.SavedPath}
                <Button
                  size="sm"
                  variant="quiet"
                  onclick={() => {
                    hidden = cards.map((card) => card.preview.ID);
                    onsaved(preview.SavedPath!);
                  }}>Saved Diff</Button
                >
              {/if}
            </div>
            {#if preview.ExternalSummary}
              <p class="mt-1.5 text-small text-muted font-mono">{preview.ExternalSummary}</p>
            {/if}
            {#each preview.Files as file, i (i)}
              {@const path = workspacePath(
                file.AfterPath || file.BeforePath || file.Path || "",
                preview.Workspace,
              )}
              <div class="flex min-h-0 flex-1 flex-col">
                <h3
                  class="mt-2 flex min-w-0 flex-none gap-2 font-mono text-code whitespace-nowrap"
                  title={path}
                >
                  {#if file.Action}<span class="flex-none text-muted">{file.Action}</span>{/if}
                  <Path {path} />
                </h3>
                <div class="min-h-0 flex-1 overflow-y-auto overscroll-contain" use:followBottom>
                  <DiffView {file} workspace={preview.Workspace} wrap />
                </div>
              </div>
            {/each}
            {#if preview.Footer}<p class="mt-1.5 flex-none text-small text-muted">
                {preview.Footer}
              </p>{/if}
          </article>
        {/each}
      </div>
    </div>
  </section>
{/if}
