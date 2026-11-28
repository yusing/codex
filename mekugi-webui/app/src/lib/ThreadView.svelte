<script lang="ts">
  import { setContext, tick, untrack } from "svelte";
  import type { InitialInput } from "./attachments";
  import Composer from "./Composer.svelte";
  import { detailLinks, type DetailLink } from "./detail-link";
  import DetailsPanel from "./DetailsPanel.svelte";
  import { matchReviewedFile } from "./diff";
  import DiffPanel from "./DiffPanel.svelte";
  import DisplayPopover from "./DisplayPopover.svelte";
  import { basename, statusText } from "./format";
  import type { Machine } from "./hub.svelte";
  import { editEntryPaths, isEditEntry } from "./kinds";
  import LiveSource from "./LiveSource.svelte";
  import Path from "./Path.svelte";
  import { preferences } from "./preferences.svelte";
  import Prompt from "./Prompt.svelte";
  import type { Entry, ThreadForkResponse } from "./protocol";
  import Roster from "./Roster.svelte";
  import { semantic } from "./semantic.svelte";
  import SessionFeatures from "./SessionFeatures.svelte";
  import SessionInfo from "./SessionInfo.svelte";
  import { streams } from "./stream.svelte";
  import ThreadDocks from "./ThreadDocks.svelte";
  import Transcript from "./Transcript.svelte";
  import { errorText } from "./transport";
  import Alert from "./ui/Alert.svelte";
  import { Badge } from "./ui/badge";
  import Button from "./ui/Button.svelte";
  import Dialog from "./ui/Dialog.svelte";
  import Dot from "./ui/Dot.svelte";
  import Field from "./ui/Field.svelte";
  import Popover from "./ui/Popover.svelte";
  import { Separator } from "./ui/separator";
  import { input as inputStyle } from "./ui/styles";
  import { open, show, view } from "./view.svelte";

  let {
    machine,
    id,
    initial,
    oninitial,
    sidebar,
    onsidebar,
  }: {
    machine: Machine;
    id: string;
    initial?: InitialInput;
    oninitial: () => void;
    sidebar: boolean;
    onsidebar: () => void;
  } = $props();

  let selectedQuestion = $state("");
  let panel = $state<"agents" | "changes" | "details" | undefined>();
  let selectedEntry = $state<Entry | undefined>();
  let selectedLink = $state<DetailLink>();
  let focusChangePath = $state<string>();
  let hiddenHooks = $state(0);
  let renameOpen = $state(false);
  let renameValue = $state("");
  let savingName = $state(false);
  let renameError = $state<string | undefined>();
  let forkError = $state<string>();
  let forking = $state(false);
  let wall = $state(Date.now());
  let narrow = $state(false);
  let composer = $state<ReturnType<typeof Composer>>();

  type OneOff = { thread: string; initial?: InitialInput; firstTurn?: string };
  let oneOffs = $state.raw(new Map<string, OneOff>());
  let selectedSide = $state.raw<{ main: string; thread: string }>();
  let sideChanging = $state(false);
  let sideError = $state<string>();
  const tabId = $props.id();
  const oneOff = $derived(oneOffs.get(id));
  const displayed = $derived(selectedSide?.main === id ? selectedSide.thread : id);
  const showingSide = $derived(displayed !== id);
  const stream = $derived(streams.current);

  $effect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const mq = window.matchMedia("(max-width: 68.74rem)");
    const sync = () => (narrow = mq.matches);
    sync();
    mq.addEventListener("change", sync);
    return () => mq.removeEventListener("change", sync);
  });

  $effect(() => {
    const timer = setInterval(() => (wall = Date.now()), 1000);
    return () => clearInterval(timer);
  });

  $effect(() => {
    const main = id;
    const thread = displayed;
    const owner = machine;
    untrack(() => {
      const side = showingSide ? oneOff : undefined;
      semantic.connect(thread, owner, main);
      streams.connect(thread, owner, (side?.initial ?? initial)?.started, side?.firstTurn);
      selectedEntry = undefined;
      selectedLink = undefined;
      renameOpen = false;
      renameError = undefined;
      savingName = false;
      forkError = undefined;
      forking = false;
    });
    return () => {
      streams.release(thread, owner);
      semantic.release(thread, owner);
    };
  });

  const session = $derived(stream.session);
  const title = $derived(
    showingSide
      ? "One-off"
      : session?.title || (session?.workspace ? basename(session.workspace) : undefined) || id,
  );
  const root = $derived(
    stream.roster.agents.find((a) => a.name === "/root") ?? stream.roster.agents[0],
  );
  const status = $derived(statusText(stream.status));
  const questionChannel = $derived(
    (semantic.prompts?.questions ?? []).flatMap((call) =>
      call.questions.map((question, index) => ({ call, question, index })),
    ),
  );
  const questionPosition = $derived(
    Math.max(
      0,
      questionChannel.findIndex((item) => item.question.id === selectedQuestion),
    ),
  );
  const currentQuestion = $derived(questionChannel[questionPosition]);
  const hasPrompts = $derived(
    !!(semantic.prompts.approvals.length || semantic.prompts.questions.length),
  );
  const net = $derived(stream.reviewOverview.counts.Net);
  const hasChanges = $derived(!!stream.review?.fileCount);

  $effect(() => {
    if (panel !== "changes" && stream.reviewCaller) stream.selectChangeCaller("");
  });

  function resolveChangePath(paths: string[]): string | undefined {
    const review = stream.review;
    if (!review?.files.length) return undefined;
    for (const path of paths) {
      const match = matchReviewedFile(review.files, path, review.workspace);
      if (match) return match.path;
    }
    return undefined;
  }

  function hasSavedChange(paths: string[]): boolean {
    return resolveChangePath(paths) !== undefined;
  }

  function openChanges(paths: string[] = []) {
    focusChangePath = resolveChangePath(paths) ?? paths[0];
    panel = "changes";
  }

  function openDetails(entry: Entry) {
    if (isEditEntry(entry) && hasChanges) {
      const target = resolveChangePath(editEntryPaths(entry));
      if (target) {
        openChanges([target]);
        return;
      }
    }
    selectedEntry = entry;
    selectedLink = undefined;
    panel = "details";
  }

  setContext(detailLinks, (link: DetailLink) => {
    selectedEntry = undefined;
    selectedLink = link;
    panel = "details";
  });

  function addReference(text: string, description: string, source: string): boolean {
    return composer?.addReference(text, description, source) ?? false;
  }

  function onkeydown(event: KeyboardEvent) {
    if ((event.metaKey || event.ctrlKey) && event.shiftKey && !event.altKey) {
      const key = event.key.toLowerCase();
      if (key === "h") {
        event.preventDefault();
        preferences.toggleHooks();
        return;
      }
      if (key === "d") {
        event.preventDefault();
        preferences.toggleDebug();
        return;
      }
    }
    if ((event.metaKey || event.ctrlKey) && !event.altKey && !event.shiftKey && event.key === ".") {
      event.preventDefault();
      panel = panel ? undefined : "agents";
    }
  }

  async function forkThread() {
    if (forking) return;
    const source = id;
    const owner = machine;
    forking = true;
    forkError = undefined;
    const res = await semantic.fork();
    if (source !== id || owner !== machine) return;
    forking = false;
    if ("id" in res) open(res.id);
    else forkError = res.error;
  }

  async function openOneOff(draft: Omit<InitialInput, "started" | "onstarted">) {
    if (sideChanging) return "One-off is already changing.";
    const owner = machine;
    const main = id;
    const previous = oneOff;
    sideChanging = true;
    sideError = undefined;
    try {
      const created = await owner.request<ThreadForkResponse>("thread/fork", {
        threadId: main,
        threadSource: "mekugi_side",
        excludeTurns: true,
      });
      if (previous) {
        try {
          await owner.request("thread/archive", { threadId: previous.thread });
        } catch (error) {
          await owner.request("thread/archive", { threadId: created.thread.id }).catch(() => {});
          throw error;
        }
      }
      const side: OneOff = { thread: created.thread.id };
      side.initial = {
        ...draft,
        started: created,
        onstarted: (turn) => {
          const current = oneOffs.get(main);
          if (current?.thread !== side.thread) return;
          oneOffs = new Map(oneOffs).set(main, { ...current, firstTurn: turn });
          stream.showFrom(side.thread, turn);
        },
      };
      oneOffs = new Map(oneOffs).set(main, side);
      if ((id === main && owner === machine) || selectedSide?.main === main) {
        selectedSide = { main, thread: side.thread };
      }
      return undefined;
    } catch (error) {
      return errorText(error, true);
    } finally {
      sideChanging = false;
    }
  }

  async function closeOneOff() {
    const side = oneOff;
    if (!side || sideChanging) return;
    const main = id;
    sideChanging = true;
    sideError = undefined;
    try {
      await machine.request("thread/archive", { threadId: side.thread });
      const remaining = new Map(oneOffs);
      remaining.delete(main);
      if (selectedSide?.main === main) selectedSide = undefined;
      oneOffs = remaining;
      if (id === main) {
        await tick();
        composer?.focus();
      }
      return undefined;
    } catch (error) {
      sideError = errorText(error, true);
      return sideError;
    } finally {
      sideChanging = false;
    }
  }

  function navigateTabs(event: KeyboardEvent) {
    const button = event.currentTarget as HTMLButtonElement;
    const tabs = [
      ...(button.parentElement?.querySelectorAll<HTMLButtonElement>('[role="tab"]') ?? []),
    ];
    const current = tabs.indexOf(button);
    if (current < 0) return;
    const next =
      event.key === "Home"
        ? 0
        : event.key === "End"
          ? tabs.length - 1
          : event.key === "ArrowRight" || event.key === "ArrowLeft"
            ? 1 - current
            : -1;
    if (next < 0) return;
    event.preventDefault();
    tabs[next]?.focus();
  }

  async function saveRename() {
    const trimmed = renameValue.trim();
    if (!trimmed || savingName) return;
    savingName = true;
    renameError = undefined;
    const err = await semantic.rename(trimmed);
    savingName = false;
    if (err) {
      renameError = err;
      return;
    }
    renameOpen = false;
  }
</script>

<svelte:window {onkeydown} />

<div class="flex min-h-0 flex-1 flex-col">
  <header
    class="flex h-13 flex-none items-center gap-2 border-b border-rule/70 px-4 max-sm:gap-1 max-sm:px-2"
  >
    {#if !sidebar}
      <Button variant="quiet" icon="sidebar" label="Show sidebar" onclick={onsidebar} />
    {/if}
    <div class="flex min-w-0 flex-1 items-center gap-2">
      <h1 class="truncate text-body font-semibold tracking-[-0.01em]">{title}</h1>
      {#if session?.workspace}
        <span class="hidden truncate font-mono text-small text-muted md:inline">
          <Path path={basename(session.workspace)} title={session.workspace} />
        </span>
      {/if}
    </div>
    <div class="flex flex-none items-center gap-1">
      {#if session}
        <DisplayPopover {hiddenHooks} predictionsAvailable={!showingSide} />
      {/if}
      <span class="flex flex-none items-center gap-1.5 px-1 text-small text-muted max-sm:hidden">
        {#if stream.connection === "open"}
          <span class="inline-flex items-center gap-1.5 font-medium text-live-ink">
            <Dot tone="open" size={6} />Live
          </span>
          <span aria-hidden="true" class="text-control">·</span>
          <span class="flex-none">{machine.label}</span>
        {:else}
          <Badge size="chrome">{machine.state.text}</Badge>
        {/if}
      </span>
      {#if session && status}
        {@const tone = stream.status.alert ? "alert" : stream.status.running ? "running" : "idle"}
        <Badge
          variant={tone === "alert" ? "destructive" : tone === "running" ? "working" : "quiet"}
          size="chrome"
          class="max-w-52 min-w-0 flex-initial max-sm:hidden"
          title={status}
        >
          <Dot {tone} /><span class="truncate">{status}</span>
        </Badge>
      {/if}
      <Separator orientation="vertical" class="mx-0.5 h-4 bg-rule max-sm:hidden" />
      <Button
        variant="quiet"
        icon="agents"
        label="Agents ({stream.roster.agents.length})"
        title="Agents"
        tooltip
        aria-pressed={panel === "agents"}
        onclick={() => (panel = panel === "agents" ? undefined : "agents")}
      >
        <span class="text-small font-medium tabular-nums">{stream.roster.agents.length}</span>
      </Button>
      <Button
        variant="quiet"
        icon="diff"
        label="Changes"
        title="Changes"
        tooltip
        aria-pressed={panel === "changes"}
        onclick={() => (panel = panel === "changes" ? undefined : "changes")}
      >
        {#if net && (net.Added || net.Removed)}
          <span class="font-mono text-small font-medium tabular-nums max-sm:hidden">
            <span class="text-positive">+{net.Added}</span>
            <span class="text-negative">−{net.Removed}</span>
          </span>
        {:else if stream.review?.fileCount}
          <span class="text-small font-medium tabular-nums max-sm:hidden"
            >{stream.review.fileCount}</span
          >
        {/if}
      </Button>
      {#if selectedEntry}
        <Button
          variant="quiet"
          size="sm"
          aria-pressed={panel === "details"}
          onclick={() => (panel = panel === "details" ? undefined : "details")}
        >
          Details
        </Button>
      {/if}
      {#if session}
        <Popover label="Session details" icon="info">
          {#snippet children(close, visible)}
            <SessionInfo {session} {root} {status} {machine} {visible} alert={stream.status.alert}>
              {#if visible}
                <SessionFeatures {machine} thread={session.thread} />
              {/if}
            </SessionInfo>
            <div class="flex items-center justify-between gap-2 border-t border-rule px-4 py-2.5">
              <div class="flex items-center gap-1.5">
                <Button
                  size="sm"
                  variant="quiet"
                  onclick={() => {
                    renameValue = title;
                    renameError = undefined;
                    renameOpen = true;
                  }}>Rename</Button
                >
                <Button
                  size="sm"
                  variant="quiet"
                  disabled={forking}
                  onclick={() => void forkThread()}>{forking ? "Forking…" : "Fork"}</Button
                >
              </div>
              <Button
                size="sm"
                variant="danger"
                disabled={!composer || stream.connection !== "open"}
                onclick={() => {
                  close();
                  composer?.openArchive();
                }}>Archive</Button
              >
            </div>
            {#if forkError}<Alert class="mx-4 mb-3">{forkError}</Alert>{/if}
          {/snippet}
        </Popover>
      {/if}
    </div>
  </header>

  {#if oneOff}
    {@const conversationTab =
      "min-w-20 rounded-t-control border-b-2 border-transparent px-3 py-1 text-muted transition-colors hover:bg-active hover:text-ink aria-selected:border-accent aria-selected:text-ink max-sm:min-h-11"}
    <div class="mx-auto flex w-full max-w-column items-center gap-1 px-4 pt-2 max-sm:px-2">
      <div
        role="tablist"
        aria-label="Conversation"
        class="flex min-w-0 flex-1 gap-1 border-b border-rule"
      >
        <button
          type="button"
          role="tab"
          class={conversationTab}
          id={tabId + "-main"}
          aria-selected={!showingSide}
          tabindex={!showingSide ? 0 : -1}
          onkeydown={navigateTabs}
          onclick={() => (selectedSide = undefined)}>Main</button
        >
        <button
          type="button"
          role="tab"
          class={conversationTab}
          id={tabId + "-oneoff"}
          aria-selected={showingSide}
          tabindex={showingSide ? 0 : -1}
          onkeydown={navigateTabs}
          onclick={() => (selectedSide = { main: id, thread: oneOff.thread })}>One-off</button
        >
      </div>
      <Button
        variant="quiet"
        size="sm"
        icon="close"
        label="Close One-off"
        disabled={sideChanging || machine.connection !== "open"}
        onclick={closeOneOff}
      />
    </div>
    {#if sideError}
      <div class="mx-auto w-full max-w-column px-4 pt-1 max-sm:px-2">
        <Alert>{sideError}</Alert>
      </div>
    {/if}
  {/if}
  {#if stream.gone}
    <div class="grid flex-1 place-content-center p-6 text-center">
      <Alert>{stream.gone.text}</Alert>
    </div>
  {:else}
    <div
      class="flex min-h-0 flex-1"
      role={oneOff ? "tabpanel" : undefined}
      aria-labelledby={oneOff ? tabId + (showingSide ? "-oneoff" : "-main") : undefined}
    >
      <div class="flex min-h-0 min-w-0 flex-1 flex-col">
        {#if stream.error}
          <div class="mx-auto w-full max-w-column px-4 pt-3 max-sm:px-2">
            <Alert>{stream.error}</Alert>
          </div>
        {/if}
        <div class="@container/thread-panes flex min-h-0 flex-1">
          <div class="flex min-h-0 min-w-0 flex-1 flex-col @min-[40rem]/thread-panes:flex-row">
            <div class="flex min-h-0 min-w-0 flex-1 flex-col">
              <Transcript
                entries={stream.transcriptEntries}
                transcript={stream}
                owner={view.agent ?? "/root"}
                workspace={session?.workspace}
                jumps={stream.jumps}
                onhiddenhooks={(count) => (hiddenHooks = count)}
                onchanges={hasChanges ? openChanges : undefined}
                hasSavedChange={hasChanges ? hasSavedChange : undefined}
                onselect={openDetails}
                onreference={stream.connection === "open" ? addReference : undefined}
              />
            </div>
            {#if preferences.liveDiff}
              {#key stream}
                <LiveSource
                  previews={stream.live}
                  owner={view.agent ?? "/root"}
                  onsaved={(path) => {
                    stream.selectChangeCaller("");
                    openChanges([path]);
                  }}
                />
              {/key}
            {/if}
          </div>
        </div>
        {#key machine.id + ":" + displayed}
          <ThreadDocks />
        {/key}
        {#if hasPrompts}
          <div class="mx-auto grid w-full max-w-column gap-2 px-4 pt-2 max-sm:px-2">
            {#each semantic.prompts.approvals.slice(0, 1) as approval (approval.id)}
              <Prompt
                item={approval}
                count={semantic.prompts.approvals.length}
                unavailable={stream.connection !== "open"}
              />
            {/each}
            {#if currentQuestion}
              {#key currentQuestion.call.id}
                <Prompt
                  item={currentQuestion.call}
                  questionIndex={currentQuestion.index}
                  position={questionPosition}
                  total={questionChannel.length}
                  onquestion={(index) => {
                    const target = currentQuestion.call.questions[index];
                    if (target) selectedQuestion = target.id;
                  }}
                  onprevious={() => {
                    const prev = questionChannel[questionPosition - 1];
                    if (prev) selectedQuestion = prev.question.id;
                  }}
                  onnext={() => {
                    const next = questionChannel[questionPosition + 1];
                    if (next) selectedQuestion = next.question.id;
                  }}
                  unavailable={stream.connection !== "open"}
                />
              {/key}
            {/if}
          </div>
        {/if}
        <Composer
          bind:this={composer}
          {machine}
          thread={displayed}
          input={stream.input}
          agent={view.agent ?? "/root"}
          role={root?.role}
          initial={showingSide ? oneOff?.initial : initial}
          oninitial={() => {
            if (showingSide && oneOff)
              oneOffs = new Map(oneOffs).set(id, { ...oneOff, initial: undefined });
            else oninitial();
          }}
          oneOffOpen={!!oneOff}
          inOneOff={showingSide}
          ononeoff={openOneOff}
          onarchive={() => (showingSide ? closeOneOff() : semantic.archive())}
          asking={hasPrompts}
          unavailable={stream.connection !== "open"
            ? machine.error || machine.state.text
            : undefined}
        />
      </div>

      {#if panel}
        {#snippet panelContent()}
          {#if panel === "agents"}
            <div class="h-full overflow-y-auto px-3 pb-4">
              <Roster
                roster={stream.roster}
                lines={stream.reviewOverview.counts.Callers}
                {net}
                selected={view.agent ?? "/root"}
                now={wall}
                loading={stream.connection === "connecting"}
                activity={(agent) => agent.activity ?? undefined}
                onselect={(name) => show(name)}
              />
            </div>
          {:else if panel === "changes"}
            <DiffPanel
              review={stream.review}
              caller={stream.review?.caller ?? ""}
              callers={stream.reviewOverview.callers}
              overview={stream.reviewOverview}
              callerLabel={(caller) => stream.label(caller)}
              oncaller={(caller) => stream.selectChangeCaller(caller)}
              includeExternal={stream.reviewIncludeExternal}
              onincludeexternal={(include) => stream.setIncludeExternalChanges(include)}
              cursor={stream.reviewCursor}
              loading={stream.reviewLoading}
              error={stream.reviewError}
              onpage={(signal) => void stream.moreChanges(signal)}
              onretry={() => stream.retryChanges()}
              focusPath={focusChangePath}
              onfocused={() => (focusChangePath = undefined)}
            />
          {:else if panel === "details" && (selectedEntry || selectedLink)}
            <DetailsPanel
              entry={selectedEntry}
              link={selectedLink}
              thread={displayed}
              {machine}
              onreference={stream.connection === "open"
                ? (text, description, source) => {
                    if (!addReference(text, description, source)) return false;
                    if (narrow) panel = undefined;
                    return true;
                  }
                : undefined}
              onclose={() => {
                panel = undefined;
                selectedEntry = undefined;
                selectedLink = undefined;
              }}
            />
          {/if}
        {/snippet}
        {#if narrow}
          <Dialog
            title={panel === "agents" ? "Agents" : panel === "changes" ? "Changes" : "Details"}
            onclose={() => (panel = undefined)}
          >
            <div class="max-h-[70vh] overflow-y-auto">
              {@render panelContent()}
            </div>
          </Dialog>
        {:else}
          <aside
            class={[
              "flex-none border-l border-rule bg-surface",
              panel === "agents"
                ? "w-side"
                : panel === "details"
                  ? "w-[min(480px,40vw)]"
                  : "w-[min(640px,46vw)]",
            ]}
          >
            {@render panelContent()}
          </aside>
        {/if}
      {/if}
    </div>
  {/if}
</div>

{#if renameOpen}
  <Dialog title="Rename thread" onclose={() => (renameOpen = false)}>
    <form
      onsubmit={(event) => {
        event.preventDefault();
        void saveRename();
      }}
    >
      <Field label="Thread name">
        <input class={inputStyle} bind:value={renameValue} required autocomplete="off" />
      </Field>
      {#if renameError}<Alert class="mt-2">{renameError}</Alert>{/if}
      <div class="mt-4 flex justify-end gap-2">
        <Button variant="quiet" onclick={() => (renameOpen = false)}>Cancel</Button>
        <Button type="submit" variant="primary" disabled={savingName || !renameValue.trim()}>
          {savingName ? "Saving…" : "Save"}
        </Button>
      </div>
    </form>
  </Dialog>
{/if}
