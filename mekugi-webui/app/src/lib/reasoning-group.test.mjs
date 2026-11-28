import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { registerHooks, stripTypeScriptTypes } from "node:module";
import { test } from "node:test";
import { compile, compileModule } from "svelte/compiler";
import { render } from "svelte/server";

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier.startsWith("$lib/"))
      specifier = new URL(specifier.slice(5), import.meta.url).href;
    if (specifier.startsWith(".") || specifier.startsWith("file:")) {
      const url = new URL(specifier, context.parentURL);
      if (url.pathname.endsWith(".js") && !existsSync(url)) {
        const source = new URL(url.href.replace(/\.js$/, ".ts"));
        if (existsSync(source)) specifier = source.href;
      } else if (!/\.(?:ts|mjs|js)$/.test(url.pathname)) {
        const index = new URL(`${url.href}/index.ts`);
        const source = new URL(`${url.href}.ts`);
        if (existsSync(index)) specifier = index.href;
        else if (existsSync(source)) specifier = source.href;
      }
    }
    return nextResolve(specifier, context);
  },
  load(url, context, nextLoad) {
    if (url.endsWith(".svelte") || /\.svelte\.(?:ts|js)$/.test(url)) {
      const source = readFileSync(new URL(url), "utf8");
      const options = { filename: new URL(url).pathname, generate: "server" };
      const result = url.endsWith(".svelte")
        ? compile(source, options)
        : compileModule(url.endsWith(".ts") ? stripTypeScriptTypes(source) : source, options);
      return { format: "module", source: result.js.code, shortCircuit: true };
    }
    return nextLoad(url, context);
  },
});

// The agent view follows the page address from the moment it loads.
globalThis.location ??= new URL("http://localhost/");
globalThis.addEventListener ??= () => {};
const { default: Transcript } = await import("./Transcript.svelte");
const { preferences } = await import("./preferences.svelte.ts");
const thought = (seq, label, live = false) => ({
  seq,
  position: seq,
  agent: "/root",
  kind: "reasoning",
  running: live,
  values: label ? [{ kind: "reasoning", label, body: `**${label}**\n\n${label} body`, live }] : [],
});

test("consecutive visible reasoning shares a latest-headed row across hidden entries", () => {
  const previous = preferences.thoughts;
  const hooks = preferences.hooks;
  try {
    preferences.setThoughts("collapsed");
    preferences.setHooks("off");
    const entries = [
      thought(1, "First"),
      thought(2),
      { seq: 3, position: 3, kind: "hook", values: [] },
      thought(4, "Latest", true),
      { seq: 5, position: 5, kind: "user", text: "Steer" },
      thought(6, "After"),
    ];
    const html = render(Transcript, { props: { entries } }).body;
    assert.equal((html.match(/data-entry="/g) ?? []).length, 3);
    assert.match(html, /2 thoughts/);
    assert.match(html, /Latest body/);
    assert.match(html, /aria-expanded="false"/);
    assert.match(html, /First body/);
    assert.match(html, /After/);
    preferences.setThoughts("expanded");
    const expanded = render(Transcript, { props: { entries } }).body;
    assert.match(expanded, /aria-expanded="true"/);
    assert.match(expanded, /First body/);
    assert.match(expanded, /Latest body/);
  } finally {
    preferences.setThoughts(previous);
    preferences.setHooks(hooks);
  }
});
