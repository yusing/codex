<script lang="ts">
  import { untrack } from "svelte";
  import { atomicAttachmentInput, type PendingAttachment } from "./attachments";
  import { hasAttachmentPlaceholder, readDraft, renderDraft } from "./draft-editor";

  let {
    id,
    label,
    value = $bindable(""),
    items,
    class: className,
    style,
    disabled,
    plain = false,
    placeholder,
    describedby,
    keyshortcuts,
    onkeydown,
    onpaste,
    onscroll,
    onselection,
  }: {
    id: string;
    label: string;
    value?: string;
    items: PendingAttachment[];
    class: string | (string | false | undefined)[];
    style?: string;
    disabled: boolean;
    plain?: boolean;
    placeholder: string;
    describedby?: string;
    keyshortcuts?: string;
    onkeydown: (event: KeyboardEvent) => void;
    onpaste: (event: ClipboardEvent) => void;
    onscroll?: (field: HTMLElement) => void;
    onselection?: (start: number, end: number) => void;
  } = $props();

  const rich = $derived(!plain && hasAttachmentPlaceholder(items));
  let textarea = $state<HTMLTextAreaElement>();
  let host = $state<HTMLDivElement>();
  let rendered: { host: HTMLDivElement; text: string; items: PendingAttachment[] } | undefined;
  let takeFocus = false;

  $effect.pre(() => {
    const nextIsRich = rich;
    const active = document.activeElement;
    takeFocus = untrack(() => active === (nextIsRich ? textarea : host));
  });

  $effect(() => {
    const field = host;
    if (
      field &&
      (rendered?.host !== field || rendered.text !== value || rendered.items !== items)
    ) {
      renderDraft(field, value, items);
      rendered = { host: field, text: value, items };
      if (document.activeElement === field) setSelectionRange(value.length, value.length);
    }
    if (takeFocus) {
      takeFocus = false;
      focus();
      setSelectionRange(value.length, value.length);
    }
  });

  export function focus() {
    (host ?? textarea)?.focus();
  }

  function richSelection(draft = host && readDraft(host)) {
    const selection = document.getSelection();
    if (!draft || !selection?.anchorNode || !selection.focusNode) return;
    const anchor = draft.offset(selection.anchorNode, selection.anchorOffset);
    const extent = draft.offset(selection.focusNode, selection.focusOffset);
    if (anchor !== undefined && extent !== undefined)
      return { start: Math.min(anchor, extent), end: Math.max(anchor, extent) };
  }

  export function selectionStart() {
    return textarea?.selectionStart ?? richSelection()?.start ?? value.length;
  }

  export function setSelectionRange(start: number, end: number) {
    if (textarea) textarea.setSelectionRange(start, end);
    else if (host) {
      const draft = readDraft(host);
      document.getSelection()?.setBaseAndExtent(...draft.point(start), ...draft.point(end));
    }
  }

  function reportTextareaSelection() {
    if (textarea) onselection?.(textarea.selectionStart, textarea.selectionEnd);
  }

  function reportRichSelection() {
    if (host && document.activeElement === host && onselection) {
      const selection = richSelection();
      if (selection) onselection(selection.start, selection.end);
    }
  }

  function onRichInput(event: Event & { currentTarget: HTMLDivElement }) {
    const field = event.currentTarget;
    const draft = readDraft(field);
    rendered = { host: field, text: draft.text, items };
    value = draft.text;
    const selection = richSelection(draft);
    if (selection) onselection?.(selection.start, selection.end);
  }
</script>

<svelte:document onselectionchange={reportRichSelection} />
<label class="sr-only" id={id + "-label"} for={rich ? undefined : id}>{label}</label>
{#if rich}
  <div
    {id}
    data-slot="input-group-control"
    data-placeholder={!value ? placeholder : undefined}
    class={[className, "overflow-auto whitespace-pre-wrap wrap-anywhere"]}
    {style}
    bind:this={host}
    contenteditable={disabled ? "false" : "plaintext-only"}
    tabindex={disabled ? -1 : 0}
    role="textbox"
    aria-labelledby={id + "-label"}
    aria-multiline="true"
    aria-disabled={disabled}
    aria-placeholder={placeholder}
    aria-describedby={describedby}
    aria-keyshortcuts={keyshortcuts}
    oninput={onRichInput}
    {onkeydown}
    {onpaste}
    onscroll={({ currentTarget }) => onscroll?.(currentTarget)}
  ></div>
{:else}
  <textarea
    {id}
    data-slot="input-group-control"
    class={className}
    {style}
    bind:this={textarea}
    bind:value
    use:atomicAttachmentInput={() => ({ text: value, items })}
    aria-describedby={describedby}
    aria-keyshortcuts={keyshortcuts}
    oninput={reportTextareaSelection}
    onselect={reportTextareaSelection}
    onselectionchange={reportTextareaSelection}
    {onkeydown}
    {onpaste}
    onscroll={({ currentTarget }) => onscroll?.(currentTarget)}
    rows="2"
    {disabled}
    {placeholder}></textarea>
{/if}

<style>
  [data-placeholder]::before {
    content: attr(data-placeholder);
    position: absolute;
    pointer-events: none;
    color: var(--muted);
  }
</style>
