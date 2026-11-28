import { attachmentPlaceholder, imageAttachment, type PendingAttachment } from "./attachments";

export function hasAttachmentPlaceholder(items: PendingAttachment[]) {
  return items.some((item) => !!attachmentPlaceholder(item));
}

function isImage(item: PendingAttachment) {
  return "path" in item
    ? !item.fileReference
    : item.reference === undefined && imageAttachment(item);
}

function placeholderColour(item: PendingAttachment, label: string): string {
  if (("file" in item && item.reference !== undefined) || label.startsWith("[Selected "))
    return "text-kind-write";
  if (isImage(item) || label.startsWith("[Image ")) return "text-marker-image";
  return "text-positive";
}

type Point = [node: Node, offset: number];

// The plaintext editing host owns native editing and undo. Read both its text
// and selection offsets from the same DOM, including browser-created line breaks.
export function readDraft(host: HTMLElement) {
  const chunks: string[] = [];
  const offsets = new Map<Node, number[]>();
  let length = 0;
  function append(value: string) {
    chunks.push(value);
    length += value.length;
  }
  function read(parent: HTMLElement) {
    const edges: number[] = [];
    let previousBlock = false;
    for (const node of parent.childNodes) {
      const block = node instanceof HTMLElement && (node.tagName === "DIV" || node.tagName === "P");
      if (edges.length && (previousBlock || block)) append("\n");
      edges.push(length);
      if (node instanceof Text) {
        offsets.set(node, [length]);
        // Blink appends a second newline node to draw the caret on the last line.
        if (node.data !== "\n" || node.nextSibling) append(node.data);
      } else if (node instanceof HTMLBRElement) {
        // A final BR is the editing host's caret placeholder, not draft text.
        if (node.nextSibling) append("\n");
      } else if (node instanceof HTMLElement) read(node);
      previousBlock = block;
    }
    edges.push(length);
    offsets.set(parent, edges);
  }
  read(host);
  function offset(node: Node, at: number) {
    const edges = offsets.get(node);
    return edges ? Math.min(length, node instanceof Text ? edges[0] + at : edges[at]) : undefined;
  }
  function point(at: number): Point {
    for (const [node, edges] of offsets) {
      if (node instanceof Text && at >= edges[0] && at <= edges[0] + node.length) {
        const token = node.parentElement?.closest<HTMLElement>("[data-draft-attachment]");
        if (!token) return [node, at - edges[0]];
        const parent = token.parentNode!;
        const index = Array.prototype.indexOf.call(parent.childNodes, token);
        return [parent, index + Number(at > edges[0])];
      }
      const index = edges.indexOf(at);
      if (node instanceof HTMLElement && !node.closest("[data-draft-attachment]") && index >= 0)
        return [node, index];
    }
    return [host, host.childNodes.length];
  }
  return { text: chunks.join(""), offset, point };
}

export function renderDraft(host: HTMLElement, text: string, items: PendingAttachment[]) {
  const ranges = items.flatMap((item) => {
    const label = attachmentPlaceholder(item);
    const start = label ? text.indexOf(label) : -1;
    return start < 0 || !label ? [] : [{ item, start, end: start + label.length }];
  });
  ranges.sort((a, b) => a.start - b.start);
  let end = 0;
  const parts = document.createDocumentFragment();
  for (const range of ranges) {
    if (range.start < end) continue;
    if (end < range.start) parts.append(text.slice(end, range.start));
    const label = text.slice(range.start, range.end);
    const token = document.createElement("span");
    token.dataset.draftAttachment = range.item.id;
    token.contentEditable = "false";
    token.className = `inline-block max-w-full truncate align-bottom font-semibold whitespace-nowrap ${placeholderColour(range.item, label)}`;
    token.textContent = label;
    parts.append(token);
    end = range.end;
  }
  if (end < text.length) parts.append(text.slice(end));
  if (text.endsWith("\n")) parts.append(document.createElement("br"));
  host.replaceChildren(parts);
}
