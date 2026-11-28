<script lang="ts">
  import { untrack } from "svelte";
  import type { Attachment } from "svelte/attachments";
  import AgentName from "./AgentName.svelte";
  import AutoPage from "./AutoPage.svelte";
  import { matchReviewedFile, preferredScrollBehavior, workspacePath } from "./diff";
  import DiffView from "./DiffView.svelte";
  import Path from "./Path.svelte";
  import type { CallerReview, Counts, ReviewedFile, SavedReview } from "./protocol";
  import Alert from "./ui/Alert.svelte";
  import Button from "./ui/Button.svelte";
  import Picker from "./ui/Picker.svelte";
  import { Skeleton } from "./ui/skeleton";
  import { caret, emptyBox } from "./ui/styles";

  let {
    review,
    caller,
    callers,
    overview,
    callerLabel,
    oncaller,
    includeExternal = false,
    onincludeexternal,
    cursor,
    loading,
    error,
    onpage,
    onretry,
    focusPath,
    onfocused,
  }: {
    review: SavedReview | null;
    caller: string;
    callers: CallerReview[];
    overview?: { fileCount: number; counts: SavedReview["counts"] };
    callerLabel: (caller: string) => string;
    oncaller: (caller: string) => void;
    includeExternal?: boolean;
    onincludeexternal?: (include: boolean) => void;
    cursor?: string;
    loading: boolean;
    error?: string;
    onpage: (demand: AbortSignal) => void;
    onretry: () => void;
    focusPath?: string;
    onfocused?: () => void;
  } = $props();

  const wrap = false;
  const files = $derived(review?.files ?? []);
  const focusIndex = $derived.by(() => {
    if (!focusPath) return -1;
    const match = matchReviewedFile(files, focusPath, review?.workspace ?? "");
    return match ? files.indexOf(match) : -1;
  });
  function formatSummary(count: number, counts?: Counts | null): string {
    if (!count) return "No saved changes";
    return [
      counts && `+${counts.Added} −${counts.Removed}`,
      `${count} ${count === 1 ? "file" : "files"}`,
    ]
      .filter(Boolean)
      .join(" · ");
  }

  const selectedCaller = $derived(
    caller ? callers.find((choice) => choice.caller === caller) : undefined,
  );
  const headerCount = $derived(selectedCaller ? selectedCaller.count : review?.fileCount);
  const net = $derived(selectedCaller ? selectedCaller.counts : review?.counts.Net);
  const callerChoices = $derived([
    {
      value: "all",
      label: "All callers",
      detail: formatSummary(
        overview?.fileCount ?? review?.fileCount ?? 0,
        overview?.counts.Net ?? review?.counts.Net,
      ),
    },
    ...callers.map((choice) => ({
      value: choice.caller,
      label: callerLabel(choice.caller),
      detail: formatSummary(choice.count, choice.counts),
    })),
  ]);
  function sourceLabel(source: string): string {
    switch (source) {
      case "ApplyPatch":
        return "apply_patch";
      case "ShellWrite":
        return "Shell";
      case "UserShell":
        return "User shell";
      case "MchangesApply":
        return "mchanges apply";
      case "MchangesRevert":
        return "mchanges revert";
      default:
        return source;
    }
  }

  // Every file starts expanded by default; an IntersectionObserver preloads
  // diff views as the reader scrolls near them so large reviews stay snappy.
  let toggled = $state<Record<string, boolean>>({});
  let nearViewport = $state<Record<string, boolean>>({});
  let scroller = $state<HTMLElement>();
  const open = (file: ReviewedFile) => toggled[file.key] ?? true;
  const anyOpen = $derived(files.some((file) => open(file)));

  function setAllOpen(nextOpen: boolean) {
    const nextState: Record<string, boolean> = {};
    for (const file of files) nextState[file.key] = nextOpen;
    toggled = nextState;
  }

  let observer: IntersectionObserver | undefined;
  const observedNodes = new Map<HTMLElement, string>();
  $effect(() => {
    const root = scroller ?? null;
    const io = new IntersectionObserver(
      (entries) => {
        let changed = false;
        const nextNear = { ...nearViewport };
        for (const entry of entries) {
          if (!entry.isIntersecting) continue;
          const key = (entry.target as HTMLElement).dataset.file;
          if (key && !nextNear[key]) {
            nextNear[key] = true;
            changed = true;
          }
        }
        if (changed) nearViewport = nextNear;
      },
      { root, rootMargin: "240px 0px" },
    );
    observer = io;
    for (const node of observedNodes.keys()) io.observe(node);
    return () => {
      io.disconnect();
      if (observer === io) observer = undefined;
    };
  });

  const observeFile =
    (key: string): Attachment<HTMLElement> =>
    (node) => {
      observedNodes.set(node, key);
      observer?.observe(node);
      return () => {
        observedNodes.delete(node);
        observer?.unobserve(node);
      };
    };

  $effect(() => {
    const viewport = scroller;
    if (focusIndex < 0 || !viewport) return;
    const preceding = files.slice(0, focusIndex + 1);
    const match = preceding[focusIndex];
    untrack(() => {
      toggled[match.key] = true;
      const ready = { ...nearViewport };
      for (const file of preceding) ready[file.key] = true;
      nearViewport = ready;
    });
    // Measure the rows up to the target before scrolling. Their remembered
    // sizes keep the target in place when offscreen deferral resumes.
    let scrollFrame = 0;
    let doneFrame = 0;
    const measureFrame = requestAnimationFrame(() => {
      scrollFrame = requestAnimationFrame(() => {
        const summary = [...viewport.querySelectorAll<HTMLElement>("[data-file]")]
          .find((node) => node.dataset.file === match.key)
          ?.querySelector("summary");
        if (summary) {
          const behavior = preferredScrollBehavior();
          if (viewport.scrollHeight > viewport.clientHeight) {
            const top = summary.getBoundingClientRect().top - viewport.getBoundingClientRect().top;
            viewport.scrollBy({ top, behavior });
          } else {
            summary.scrollIntoView({ block: "start", behavior });
          }
        }
        doneFrame = requestAnimationFrame(() => onfocused?.());
      });
    });
    return () => {
      cancelAnimationFrame(measureFrame);
      cancelAnimationFrame(scrollFrame);
      cancelAnimationFrame(doneFrame);
    };
  });
</script>

<div class="flex h-full min-h-0 flex-col" aria-busy={loading}>
  <div class="flex min-h-11 flex-wrap items-center gap-2 border-b border-rule px-4 py-2 text-small">
    <span class="font-semibold">
      Saved Diff {#if headerCount}<span class="font-normal text-muted tabular-nums"
          >({headerCount})</span
        >{/if}
    </span>
    {#if callers.length > 0}
      <Picker
        label="Caller"
        value={caller || "all"}
        groups={[{ choices: callerChoices }]}
        onchange={(choice) => oncaller(choice === "all" ? "" : choice)}
      />
    {/if}
    {#if onincludeexternal}
      <Button
        variant={includeExternal ? "default" : "quiet"}
        size="sm"
        class="text-muted"
        aria-pressed={includeExternal}
        onclick={() => onincludeexternal(!includeExternal)}
      >
        {includeExternal ? "External: on" : "External: off"}
      </Button>
    {/if}
    {#if net}
      <span class="font-mono tabular-nums">
        <span class="text-positive">+{net.Added}</span>
        <span class="text-negative">−{net.Removed}</span>
      </span>
    {/if}
    {#if review?.coverage}<span class="text-muted">{review.coverage}</span>{/if}
    {#if files.length > 1}
      <Button
        variant="quiet"
        size="sm"
        class="ml-auto text-muted"
        onclick={() => setAllOpen(!anyOpen)}
      >
        {anyOpen ? "Collapse all" : "Expand all"}
      </Button>
    {/if}
  </div>
  <div bind:this={scroller} class="min-h-0 flex-1 overflow-y-auto overscroll-contain px-4 py-3">
    {#if files.length}
      <ul class="grid gap-2">
        {#each files as file, index (file.key)}
          {@const eager = index < 2}
          {@const isOpen = open(file)}
          {@const ready = eager || nearViewport[file.key]}
          <li
            data-file={file.key}
            class:deferred-file={isOpen && !eager}
            class:measuring-file={index <= focusIndex}
            {@attach observeFile(file.key)}
          >
            <details
              class="group/disclosure"
              open={isOpen}
              ontoggle={(e) => {
                const now = (e.currentTarget as HTMLDetailsElement).open;
                if (now !== open(file)) toggled[file.key] = now;
                if (now && !nearViewport[file.key])
                  nearViewport = { ...nearViewport, [file.key]: true };
              }}
            >
              <summary
                class="flex min-h-9 cursor-pointer items-center gap-2 rounded-control bg-raised/60 px-2.5 py-1 text-small transition-colors hover:bg-active/80 {caret}"
              >
                {#if file.status}<span class="flex-none font-mono text-code text-muted"
                    >{file.status}</span
                  >{/if}
                <span class="min-w-0 flex-1 truncate font-mono text-code font-medium"
                  ><Path path={workspacePath(file.path, review?.workspace ?? "")} /></span
                >
                <span class="flex-none font-mono tabular-nums">
                  <span class={file.counts.Added ? "text-positive" : "text-muted"}
                    >+{file.counts.Added}</span
                  >
                  <span class={file.counts.Removed ? "text-negative" : "text-muted"}
                    >−{file.counts.Removed}</span
                  >
                </span>
              </summary>
              {#if isOpen}
                <div class="pb-1">
                  {#if file.origins.length}
                    <ul class="flex flex-wrap gap-x-3 gap-y-1 px-2.5 pt-1.5 text-small text-muted">
                      {#each file.origins as origin (origin.Change)}
                        <li class="flex items-center gap-1.5">
                          {#if origin.Caller}<AgentName
                              name={origin.Caller}
                              current=""
                              quiet
                            />{/if}
                          <span>{sourceLabel(origin.Source)}</span>
                          <span class="font-mono">{origin.Change}</span>
                        </li>
                      {/each}
                    </ul>
                  {/if}
                  {#if ready}
                    {#each file.chunks as chunk (chunk.Key)}
                      <DiffView file={chunk.Review} workspace={review?.workspace ?? ""} {wrap} />
                    {/each}
                  {:else}
                    <div
                      class="mt-1.5 grid gap-1.5 rounded-card bg-raised p-3"
                      aria-label="Loading diff"
                    >
                      <Skeleton class="h-3.5 w-3/4 bg-active" />
                      <Skeleton class="h-3.5 w-1/2 bg-active" />
                      <Skeleton class="h-3.5 w-5/6 bg-active" />
                    </div>
                  {/if}
                </div>
              {/if}
            </details>
          </li>
        {/each}
      </ul>
    {:else if loading && !review}
      <Skeleton class="h-9 bg-active" />
    {:else if !error}
      <p class={emptyBox}>No file changes recorded for this thread yet.</p>
    {/if}
    {#if error}
      <Alert>{error}<Button size="sm" variant="quiet" onclick={onretry}>Retry</Button></Alert>
    {/if}
    {#if cursor}<AutoPage {cursor} ready={!loading && !error} {onpage} />{/if}
  </div>
</div>

<style>
  .deferred-file {
    content-visibility: auto;
    /* Reserve the summary height before the browser has measured the file. */
    contain-intrinsic-block-size: auto 2.25rem;
  }

  .deferred-file:focus-within,
  .deferred-file.measuring-file {
    content-visibility: visible;
  }
</style>
