<script lang="ts">
  import { getContext } from "svelte";
  import { detailLinks, parseDetailLink, type DetailLink } from "./detail-link";
  import { highlightWithin } from "./highlight";
  import { block, inline as renderInline, userBlock } from "./markdown";

  // Rendered Markdown from recorded messages, styled by the prose component
  // class since no template declares its elements.
  let {
    text,
    quiet = false,
    inline = false,
    breaks = false,
    userMessage = false,
    spans,
    context,
  }: {
    text: string;
    quiet?: boolean;
    inline?: boolean;
    breaks?: boolean;
    userMessage?: boolean;
    spans?: import("./protocol").Entry["spans"];
    context?: import("markdown-it").Env;
  } = $props();
  const html = $derived(
    inline
      ? renderInline(text, context)
      : userMessage
        ? userBlock(text, spans)
        : block(text, context, breaks),
  );
  const openDetails = getContext<((link: DetailLink) => void) | undefined>(detailLinks);
  let linkError = $state<string>();
  // Fenced code names its language as a language-NAME class.
  const fences = (rendered: string) => (node: HTMLElement) => {
    if (rendered.includes("<pre>"))
      highlightWithin(node, "pre > code", (code) => /language-(\S+)/.exec(code.className)?.[1]);
  };
  function onLinkClick(event: MouseEvent) {
    const target =
      event.target instanceof Element
        ? event.target.closest<HTMLElement>("[data-file-link], [data-run-link]")
        : null;
    if (!target) return;
    const value = target.dataset.fileLink || target.dataset.runLink;
    if (!value) return;
    event.preventDefault();
    const link = parseDetailLink(value);
    if (link && openDetails) {
      linkError = undefined;
      openDetails(link);
    } else {
      linkError = link
        ? "Open a thread to view its linked details."
        : "The detail link is invalid.";
    }
  }
  const links = (node: HTMLElement) => {
    node.addEventListener("click", onLinkClick);
    return () => node.removeEventListener("click", onLinkClick);
  };
</script>

{#if inline}
  <span class={["prose-inline", quiet && "text-muted"]} {@attach links}>
    {@html html}
  </span>
  {#if linkError}<span class="ml-2 text-small text-negative" role="alert">{linkError}</span>{/if}
{:else}
  <div class={["prose", quiet && "text-muted"]} {@attach fences(html)} {@attach links}>
    {@html html}
  </div>
  {#if linkError}<p class="mt-2 text-small text-negative" role="alert">{linkError}</p>{/if}
{/if}
