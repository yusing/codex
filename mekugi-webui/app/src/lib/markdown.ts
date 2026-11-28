import MarkdownIt, { type Env } from "markdown-it";

// Recorded text is untrusted. Local links use the machine's shared file
// interaction instead of navigating the browser filesystem.
const encoder = new TextEncoder();
const decoder = new TextDecoder();

const web = /^(?:https?:\/\/|mailto:)/i;
const runPrefix = "mekugi:run/";
const local = (href: string) =>
  !/^[#?]/.test(href) &&
  (!/^[a-z][a-z\d+.-]*:/i.test(href) || href.startsWith("/") || href.startsWith("file:"));

function createMarkdown(breaks: boolean) {
  const instance = new MarkdownIt({ html: false, linkify: true, breaks });
  const validateLink = instance.validateLink.bind(instance);
  instance.validateLink = (href) => href.startsWith("file:") || validateLink(href);
  const defaultLinkOpen =
    instance.renderer.rules.link_open ??
    ((tokens, i, options, _env, self) => self.renderToken(tokens, i, options));

  instance.renderer.rules.link_open = (tokens, i, options, env, self) => {
    const href = String(tokens[i].attrGet("href") ?? "");
    if (href.startsWith(runPrefix)) {
      tokens[i].attrSet("href", "#activity-details");
      tokens[i].attrSet("class", "file-link");
      tokens[i].attrSet("data-run-link", href);
      tokens[i].attrSet("title", href);
      return defaultLinkOpen(tokens, i, options, env, self);
    }
    if (!web.test(href)) {
      tokens[i].attrs = [];
      tokens[i].tag = local(href) ? "a" : "span";
      if (local(href)) {
        tokens[i].attrSet("href", "#activity-details");
        let path = href;
        if (!href.startsWith("file:")) {
          try {
            path = decodeURIComponent(href);
          } catch {
            /* Preserve a literal malformed escape. */
          }
        }
        tokens[i].attrSet("data-file-link", path);
      }
      tokens[i].attrSet("class", "file-link");
      tokens[i].attrSet("title", href);
      const close = tokens.slice(i + 1).find((t) => t.type === "link_close");
      if (close) close.tag = tokens[i].tag;
      return self.renderToken(tokens, i, options);
    }
    tokens[i].attrSet("target", "_blank");
    tokens[i].attrSet("rel", "noopener noreferrer");
    return defaultLinkOpen(tokens, i, options, env, self);
  };

  // Images become links, so opening a recording never fetches from the hosts it names.
  instance.renderer.rules.image = (tokens, i) => {
    const src = String(tokens[i].attrGet("src") ?? "");
    const alt = instance.utils.escapeHtml(tokens[i].content || src);
    return web.test(src)
      ? `<a href="${instance.utils.escapeHtml(src)}" target="_blank" rel="noopener noreferrer">Image: ${alt}</a>`
      : `<span class="file-link" title="${instance.utils.escapeHtml(src)}">Image: ${alt}</span>`;
  };

  return instance;
}

const md = createMarkdown(false);
const mdBreaks = createMarkdown(true);
const mdUser = createMarkdown(false);
// An unmarked line after a user quote belongs to the user's own message.
mdUser.block.ruler.before(
  "paragraph",
  "end_explicit_quote",
  (_state, _start, _end, silent) => silent,
  {
    alt: ["blockquote"],
  },
);

// Sections share their original document's link definitions.
export function referenceContext(text: string): Env {
  const context = {};
  md.parse(text, context);
  return context;
}

export const block = (text: string, context?: Env, breaks = false) =>
  (breaks ? mdBreaks : md).render(text, context);
export const inline = (text: string, context?: Env) => md.renderInline(text, context);
export const literal = (text: string) => md.utils.escapeHtml(text);

const plainBlockTokens = new Set(["paragraph_open", "inline", "paragraph_close", "code_block"]);
const plainInlineTokens = new Set(["text", "softbreak", "hardbreak"]);
const backslashHardbreak = /(?:^|[^\\])(?:\\\\)*\\\r?\n/;

// True when text contains Markdown syntax (fences, lists, headings, tables,
// backslash hard breaks, emphasis, links, or inline code) rather than an
// aligned or right-padded plain-text mockup.
export function isMarkdownPreview(text: string): boolean {
  const tokens = md.parse(text, {});
  const hasBackslashBreak = backslashHardbreak.test(text);
  for (const token of tokens) {
    if (!plainBlockTokens.has(token.type)) return true;
    if (token.type !== "inline" || !token.children) continue;
    for (const child of token.children) {
      if (!plainInlineTokens.has(child.type)) return true;
      if (child.type === "hardbreak" && hasBackslashBreak) return true;
    }
  }
  return false;
}

// True when text contains block-level Markdown (fences, lists, quotes,
// headings, tables, multiple paragraphs, or line breaks with visible block
// output) that should render as block Markdown rather than inline.
export function hasBlockMarkdown(text: string, context?: Env): boolean {
  const tokens = md.parse(text, context ? { ...context } : {});
  if (tokens.length === 0) return false;
  let paragraphs = 0;
  for (const token of tokens) {
    if (token.type === "paragraph_open") {
      paragraphs++;
      if (paragraphs > 1) return true;
    } else if (token.type !== "inline" && token.type !== "paragraph_close") {
      return true;
    }
  }
  return text.includes("\n");
}

// Classified labels already quote operands as Markdown code spans. Keep their
// boundaries rather than splitting punctuation inside commands or search patterns.
export function toolLabel(text: string, verb: string): string {
  const tokens = md.parseInline(text, {});
  for (const token of tokens) {
    const children = token.children ?? [];
    let operand = 0;
    for (let i = 0; i < children.length; i++) {
      const child = children[i];
      if (child.type === "code_inline") {
        child.attrSet("class", verb === "Search" && operand === 0 ? "text-accent" : "tool-operand");
        operand++;
      } else if (
        child.type === "text" &&
        child.content === ", " &&
        children[i - 1]?.type === "code_inline" &&
        children[i + 1]?.type === "code_inline"
      )
        child.content = " ";
    }
  }
  return md.renderer.render(tokens, md.options, {});
}

// Only published byte spans are markers. Identical unbound text stays ordinary.
export function userBlock(text: string, spans?: import("./protocol").Entry["spans"]): string {
  if (!spans?.length) return mdUser.render(text);
  const bytes = encoder.encode(text);
  let prefix = "[mekugi-bound:";
  while (text.includes(prefix)) prefix += ":";
  const markers: { key: string; html: string }[] = [];
  const colours = ["text-marker-image", "text-positive", "text-kind-write", "text-accent"];
  let at = 0,
    source = "";
  for (const span of spans) {
    if (
      span.Start < at ||
      span.End <= span.Start ||
      span.End > bytes.length ||
      (bytes[span.Start] & 0xc0) === 0x80 ||
      (span.End < bytes.length && (bytes[span.End] & 0xc0) === 0x80)
    )
      continue;
    const colour = colours[span.Kind];
    if (!colour) continue;
    const key = `${prefix}${markers.length}]`;
    markers.push({
      key,
      html: `<span class="font-semibold whitespace-nowrap ${colour}">${md.utils.escapeHtml(decoder.decode(bytes.slice(span.Start, span.End)))}</span>`,
    });
    source += decoder.decode(bytes.slice(at, span.Start)) + key;
    at = span.End;
  }
  source += decoder.decode(bytes.slice(at));
  return mdUser
    .render(source)
    .split(/(<[^>]*>)/)
    .map((part, i) => {
      if (i % 2) return part;
      for (const marker of markers) part = part.replaceAll(marker.key, marker.html);
      return part;
    })
    .join("");
}
