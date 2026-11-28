<script lang="ts">
  import BrainIcon from "@lucide/svelte/icons/brain";
  import type { Snippet } from "svelte";
  import { Spinner } from "../spinner";
  import * as Trace from "../trace";

  let {
    heading,
    label,
    running = false,
    defaultOpen = false,
    children,
    liveContent,
  }: {
    heading?: string;
    label?: string;
    running?: boolean;
    defaultOpen?: boolean;
    children?: Snippet;
    liveContent?: Snippet;
  } = $props();
  let open = $derived(defaultOpen);
</script>

{#snippet line()}
  <Trace.Line status={running ? "running" : undefined} class="min-h-[26px] gap-2 text-small">
    <Trace.Icon class={["size-3.5", running ? "text-working-ink" : "text-muted"]}>
      {#if running}
        <Spinner class="size-3.5" aria-label="Reasoning in progress" />
      {:else}
        <BrainIcon />
      {/if}
    </Trace.Icon>
    {#if heading}<span class="shrink-0 font-medium text-muted">{heading}</span>{/if}
    {#if label}<Trace.Title class="font-normal text-ink/90">{label}</Trace.Title>{/if}
  </Trace.Line>
{/snippet}

<Trace.Root data-ai-element="reasoning" class="gap-0">
  {#if running && !liveContent}
    {#if label}{@render line()}{/if}
    {#if children}
      <div class="mt-1 mb-2 ml-[6px] border-l border-rule pl-3.5 text-ink">
        {@render children()}
      </div>
    {/if}
  {:else if children}
    <Trace.Collapsible bind:open>
      <Trace.CollapsibleTrigger
        class="min-h-[26px] rounded-control py-0.5 pr-1.5 transition-colors hover:text-ink pointer-coarse:min-h-9"
      >
        {@render line()}
      </Trace.CollapsibleTrigger>
      <!-- Keep nested thoughts mounted so closing the group retains each choice. -->
      <Trace.CollapsibleContent class="mt-1 mb-2 ml-[6px] border-l border-rule pl-3.5 text-ink">
        {@render children()}
      </Trace.CollapsibleContent>
    </Trace.Collapsible>
    {#if liveContent}
      <div class="mt-1 mb-2 ml-[6px] border-l border-rule pl-3.5 text-ink">
        {@render liveContent()}
      </div>
    {/if}
  {:else}
    {@render line()}
  {/if}
</Trace.Root>
