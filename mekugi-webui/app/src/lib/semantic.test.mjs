import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { registerHooks, stripTypeScriptTypes } from "node:module";
import { test } from "node:test";
import { compileModule } from "svelte/compiler";

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (
      specifier.startsWith("./") &&
      context.parentURL?.includes("/app/src/lib/") &&
      !/\.(?:ts|mjs|js)$/.test(specifier)
    ) {
      specifier += ".ts";
    }
    return nextResolve(specifier, context);
  },
  load(url, context, nextLoad) {
    if (url.endsWith(".svelte.ts")) {
      return {
        format: "module",
        source: compileModule(stripTypeScriptTypes(readFileSync(new URL(url), "utf8")), {
          filename: new URL(url).pathname,
          generate: "client",
        }).js.code,
        shortCircuit: true,
      };
    }
    return nextLoad(url, context);
  },
});

globalThis.localStorage = {
  length: 0,
  key: () => null,
  getItem: () => null,
  setItem: () => {},
  removeItem: () => {},
};
globalThis.location = { hash: "", pathname: "/", search: "" };
globalThis.history = { pushState: () => {}, replaceState: () => {} };
globalThis.addEventListener = () => {};

const {
  ThreadControl,
  questionAnswers,
  questionDraft,
  serverRequestToPrompt,
  skillsAfterEdit,
  toNativeUserInput,
} = await import("./semantic.svelte.ts");
const { Stream, ThreadStreams, itemToEntry } = await import("./stream.svelte.ts");
const { TransportError } = await import("./transport.ts");

function fakeMachine(handlers = {}) {
  const notificationListeners = new Set();
  const requestListeners = new Set();
  const openListeners = new Set();
  const requests = [];
  const responses = [];
  const errorResponses = [];
  return {
    id: "m1",
    label: "Test Box",
    connection: "open",
    requests,
    responses,
    errorResponses,
    onNotification(fn) {
      notificationListeners.add(fn);
      return () => notificationListeners.delete(fn);
    },
    onServerRequest(fn) {
      requestListeners.add(fn);
      return () => requestListeners.delete(fn);
    },
    retireThreadPrompts() {},
    onOpen(fn) {
      openListeners.add(fn);
      return () => openListeners.delete(fn);
    },
    async request(method, params) {
      requests.push({ method, params });
      if (handlers[method]) return handlers[method](params);
      return {};
    },
    async resource(kind, params) {
      requests.push({ kind, params });
      if (handlers[kind]) return handlers[kind](params);
      return {};
    },
    respond(id, result) {
      responses.push({ id, result });
    },
    respondError(id, code, message) {
      errorResponses.push({ id, code, message });
    },
    emitNotification(n) {
      for (const fn of notificationListeners) fn(n);
    },
    emitOpen() {
      for (const fn of openListeners) fn();
    },
    emitServerRequest(r) {
      for (const fn of requestListeners) fn(r);
    },
  };
}

function resumedThread(id, initialTurnsPage) {
  return { thread: { id, cwd: "/repo", status: { type: "idle" }, turns: [] }, initialTurnsPage };
}

function transcriptTurnPage(last, first, nextCursor) {
  return {
    nextCursor,
    data: Array.from({ length: last - first + 1 }, (_, index) => {
      const n = last - index;
      return {
        id: `t${n}`,
        status: n === 37 ? "inProgress" : "completed",
        items: [{ id: `m${n}`, type: "agentMessage", text: `turn ${n}` }],
      };
    }),
  };
}

test("toNativeUserInput converts text and attachments to native UserInput items", () => {
  const input = toNativeUserInput("Hello", [
    { source: "path", path: "docs/diagram.png" },
    { source: "path", path: "docs/release notes.md", fileReference: true },
    { source: "upload", name: "shot.png", data: "data:image/png;base64,AA==" },
    { source: "upload", name: "clip.png", mime: "image/png", data: "iVBORw==" },
    { source: "upload", name: "photo.jpg", data: "/9j/4A==" },
  ]);
  assert.deepEqual(input, [
    { type: "text", text: "Hello", text_elements: [] },
    { type: "localImage", path: "docs/diagram.png" },
    { type: "text", text: "@'docs/release notes.md'", text_elements: [] },
    { type: "image", url: "data:image/png;base64,AA==" },
    { type: "image", url: "data:image/png;base64,iVBORw==" },
    { type: "image", url: "data:image/jpeg;base64,/9j/4A==" },
  ]);
});

test("attachment placeholders are byte-scoped in the input without repeating a file mention", () => {
  const mention = "@'notes.md'";
  const image = "[Image photo.png]";
  const text = `猫 ${mention} ${image}`;
  assert.deepEqual(
    toNativeUserInput(text, [
      { source: "path", path: "photo.png", placeholder: image },
      { source: "path", path: "notes.md", fileReference: true, placeholder: mention },
    ]),
    [
      {
        type: "text",
        text,
        text_elements: [
          { byteRange: { start: 4, end: 15 }, placeholder: mention },
          { byteRange: { start: 16, end: 33 }, placeholder: image },
        ],
      },
      { type: "localImage", path: "photo.png" },
    ],
  );
});

test("selected text reaches the model but live and saved messages show only its marker", () => {
  const text = "猫 [Selected dialog 2]";
  const input = toNativeUserInput(text, [
    {
      source: "selection",
      placeholder: "[Selected dialog 2]",
      name: "dialog",
      path: "src/example.rs",
      data: "日本語\n```",
    },
  ]);
  assert.equal(
    input[1].text,
    "\n\n---\nUser selections:\n- dialog 2 (src/example.rs):\n  ````\n  日本語\n  ```\n  ````",
  );
  for (const content of [input, [{ ...input[0], text: text + input[1].text }]]) {
    const entry = itemToEntry({ type: "userMessage", id: "u", content }, 1, 1, "turn");
    assert.equal(entry.text, text);
    assert.deepEqual(entry.spans, [{ Start: 4, End: 23, Kind: 2 }]);
  }
});

test("re-enabling predictions ignores the previous reply for the same turn", async () => {
  const replies = [];
  const machine = fakeMachine({
    "thread/prediction/request": () => {
      const reply = Promise.withResolvers();
      replies.push(reply);
      return reply.promise;
    },
  });
  const control = new ThreadControl();
  control.connect("main", machine);
  const completed = {
    method: "turn/completed",
    params: { threadId: "main", turn: { id: "turn", status: "completed" } },
  };
  control.setPredictions(true);
  machine.emitNotification(completed);
  control.setPredictions(false);
  control.setPredictions(true);
  machine.emitNotification(completed);
  assert.equal(replies.length, 2);
  replies[1].resolve({ text: "Current suggestion" });
  replies[0].resolve({ text: "Stale suggestion" });
  await new Promise(setImmediate);
  assert.equal(control.prediction, "Current suggestion");
  control.release();
});

function countChangedEntries(before, after) {
  return before.reduce((acc, entry, idx) => acc + (entry === after[idx] ? 0 : 1), 0);
}

async function measureSingleDeltaRegenerations(historyCount) {
  const historyTurns = Array.from({ length: historyCount }, (_, idx) => ({
    id: `hist-${idx}`,
    rootTurnId: null,
    items: [
      {
        type: "agentMessage",
        id: `hist-msg-${idx}`,
        text: `History ${idx}`,
        moderation: null,
        phase: null,
        memoryCitation: null,
        delivery: null,
        questions: null,
      },
    ],
    itemsView: "full",
    status: "completed",
    error: null,
    startedAt: null,
    completedAt: null,
    durationMs: null,
  }));
  const liveTurn = {
    id: "turn-live",
    rootTurnId: null,
    items: [
      {
        type: "agentMessage",
        id: "live-msg",
        text: "Hel",
        moderation: null,
        phase: null,
        memoryCitation: null,
        delivery: null,
        questions: null,
      },
      { type: "plan", id: "live-plan", text: "Step 1" },
      {
        type: "commandExecution",
        id: "live-cmd",
        command: "echo hi",
        cwd: "/repo",
        processId: null,
        status: "inProgress",
        commandActions: [],
        aggregatedOutput: "out1\n",
        exitCode: null,
        durationMs: null,
      },
    ],
    itemsView: "full",
    status: "inProgress",
    error: null,
    startedAt: null,
    completedAt: null,
    durationMs: null,
  };
  const olderTurn = {
    id: "hist-older",
    rootTurnId: null,
    items: [
      {
        type: "agentMessage",
        id: "hist-older-msg",
        text: "Earlier history",
        moderation: null,
        phase: null,
        memoryCitation: null,
        delivery: null,
        questions: null,
      },
    ],
    itemsView: "full",
    status: "completed",
    error: null,
    startedAt: null,
    completedAt: null,
    durationMs: null,
  };
  const machine = fakeMachine({
    "thread/resume": () => ({
      thread: {
        id: "t-delta",
        cwd: "/repo",
        status: { type: "active", activeFlags: [] },
        turns: [],
      },
      initialTurnsPage: {
        data: [liveTurn, ...historyTurns.toReversed()],
        nextCursor: "older-cursor",
      },
    }),
    "thread/turns/list": () => ({ data: [olderTurn], nextCursor: null }),
    "thread/subtree/list": () => ({ data: [] }),
  });
  const s = new Stream();
  s.connect("t-delta", machine);
  await new Promise(setImmediate);

  const snapshotEntries = () => s.main.order.map((seq) => s.entry(seq));

  const feedBefore = s.main;
  const beforeText = snapshotEntries();
  machine.emitNotification({
    method: "item/agentMessage/delta",
    params: { threadId: "t-delta", turnId: "turn-live", itemId: "live-msg", delta: "lo" },
  });
  const afterText = snapshotEntries();
  assert.equal(s.main, feedBefore);
  const textRegenerated = countChangedEntries(beforeText, afterText);
  assert.equal(textRegenerated, 1);

  const beforePlan = snapshotEntries();
  machine.emitNotification({
    method: "item/plan/delta",
    params: { threadId: "t-delta", turnId: "turn-live", itemId: "live-plan", delta: "\nStep 2" },
  });
  const afterPlan = snapshotEntries();
  assert.equal(s.main, feedBefore);
  assert.equal(countChangedEntries(beforePlan, afterPlan), 1);

  const beforeCmd = snapshotEntries();
  machine.emitNotification({
    method: "item/commandExecution/outputDelta",
    params: { threadId: "t-delta", turnId: "turn-live", itemId: "live-cmd", delta: "out2\n" },
  });
  const afterCmd = snapshotEntries();
  assert.equal(s.main, feedBefore);
  assert.equal(countChangedEntries(beforeCmd, afterCmd), 1);

  // Load older turns and verify subsequent deltas still update only the affected entry in place.
  assert.equal(await s.earlier(), true);
  const feedAfterEarlier = s.main;
  const beforeAfterEarlier = snapshotEntries();
  machine.emitNotification({
    method: "item/agentMessage/delta",
    params: { threadId: "t-delta", turnId: "turn-live", itemId: "live-msg", delta: "!" },
  });
  const afterAfterEarlier = snapshotEntries();
  assert.equal(s.main, feedAfterEarlier);
  assert.equal(countChangedEntries(beforeAfterEarlier, afterAfterEarlier), 1);

  const liveMsgSeq = s.main.order.at(-3);
  const livePlanSeq = s.main.order.at(-2);
  assert.equal(s.entry(liveMsgSeq).text, "Hello!");
  assert.equal(s.entry(livePlanSeq).text, "Step 1\nStep 2");
  assert.equal(s.commandExecution("turn-live", "live-cmd").aggregatedOutput, "out1\nout2\n");
  s.close();
  return textRegenerated;
}

test("streamed text, plan, and command deltas update only the affected entry across loaded history", async () => {
  const small = await measureSingleDeltaRegenerations(2);
  const large = await measureSingleDeltaRegenerations(40);
  assert.equal(small, 1);
  assert.equal(large, small);
});

test("thread open skips background-terminal lookups for members without processes", async () => {
  const root = {
    id: "root",
    cwd: "/repo",
    status: { type: "idle" },
    extra: { skills: [], backgroundTerminalCount: 0 },
    turns: [],
  };
  const activeWithProc = {
    id: "child-proc",
    parentThreadId: "root",
    cwd: "/repo",
    status: { type: "active", activeFlags: [] },
    extra: { skills: [], backgroundTerminalCount: 1 },
    turns: [],
  };
  const idleEmpty1 = {
    id: "child-idle-1",
    parentThreadId: "root",
    cwd: "/repo",
    status: { type: "idle" },
    extra: { skills: [], backgroundTerminalCount: 0 },
    turns: [],
  };
  const idleEmpty2 = {
    id: "child-idle-2",
    parentThreadId: "root",
    cwd: "/repo",
    status: { type: "idle" },
    extra: { skills: [], backgroundTerminalCount: 0 },
    turns: [],
  };
  const unloaded1 = {
    id: "child-unloaded-1",
    parentThreadId: "root",
    cwd: "/repo",
    status: { type: "notLoaded" },
    turns: [],
  };
  const unloaded2 = {
    id: "child-unloaded-2",
    parentThreadId: "root",
    cwd: "/repo",
    status: { type: "notLoaded" },
    turns: [],
  };
  const processLists = new Map([
    ["child-proc", [{ processId: "proc-1", turnId: "t1", itemId: "i1", command: "sleep 60" }]],
  ]);
  const machine = fakeMachine({
    "thread/resume": () => ({ thread: root }),
    "thread/subtree/list": () => ({
      data: [root, activeWithProc, idleEmpty1, idleEmpty2, unloaded1, unloaded2],
    }),
    "thread/backgroundTerminals/list": ({ threadId }) => ({
      data: processLists.get(threadId) ?? [],
    }),
  });
  const bgRequests = () =>
    machine.requests
      .filter((req) => req.method === "thread/backgroundTerminals/list")
      .map((req) => req.params.threadId);

  const stream = new Stream();
  stream.connect(root.id, machine);
  await new Promise(setImmediate);
  assert.deepEqual(bgRequests(), ["child-proc"]);
  assert.deepEqual(
    stream.processes.map(({ threadId, processId }) => [threadId, processId]),
    [["child-proc", "proc-1"]],
  );

  // A newly joined member with no background terminals incurs 0 list RPCs.
  machine.emitNotification({
    method: "thread/started",
    params: {
      thread: {
        id: "child-joined",
        parentThreadId: "root",
        cwd: "/repo",
        status: { type: "active", activeFlags: [] },
        extra: { skills: [], backgroundTerminalCount: 0 },
        turns: [],
      },
    },
  });
  await new Promise(setImmediate);
  assert.deepEqual(bgRequests(), ["child-proc"]);

  // When the joined member spawns a background terminal, the change notification loads it.
  processLists.set("child-joined", [
    { processId: "proc-2", turnId: "t2", itemId: "i2", command: "npm run dev" },
  ]);
  machine.emitNotification({
    method: "thread/backgroundTerminals/changed",
    params: { threadId: "child-joined" },
  });
  await new Promise(setImmediate);
  assert.deepEqual(bgRequests(), ["child-proc", "child-joined"]);
  assert.deepEqual(
    stream.processes.map(({ threadId, processId }) => [threadId, processId]),
    [
      ["child-proc", "proc-1"],
      ["child-joined", "proc-2"],
    ],
  );

  // Unloading a member clears its processes without another RPC.
  machine.emitNotification({
    method: "thread/status/changed",
    params: { threadId: "child-proc", status: { type: "notLoaded" } },
  });
  assert.deepEqual(bgRequests(), ["child-proc", "child-joined"]);
  assert.deepEqual(
    stream.processes.map(({ threadId, processId }) => [threadId, processId]),
    [["child-joined", "proc-2"]],
  );
  stream.close();
});

test("queued sendInput supplies clientUserMessageId and refreshQueue reads response.data", async () => {
  const queuedItem = {
    id: "q1",
    input: [{ type: "text", text: "Run tests", text_elements: [] }],
    clientUserMessageId: "c1",
    createdAt: 1700000000,
  };
  const machine = fakeMachine({
    "thread/queue/list": () => ({ data: [queuedItem], nextCursor: null }),
    "thread/queue/add": () => ({ queuedSubmission: queuedItem }),
  });
  const control = new ThreadControl();
  control.connect("t1", machine);
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(control.queued.length, 1);
  assert.equal(control.queued[0].id, "q1");

  const err = await control.sendInput("Run tests", [], { queue: true });
  assert.equal(err, undefined);
  const addReq = machine.requests.find((r) => r.method === "thread/queue/add");
  assert.ok(addReq);
  assert.equal(addReq.params.threadId, "t1");
  assert.equal(typeof addReq.params.clientUserMessageId, "string");
  assert.ok(addReq.params.clientUserMessageId.length > 0);
  control.release();
});

test("browser shell drafts execute immediately or queue without model input", async () => {
  const machine = fakeMachine();
  const control = new ThreadControl();
  control.connect("t1", machine);

  assert.equal(await control.sendInput("!printf '%s' x "), undefined);
  assert.equal(await control.sendInput("!pwd", [], { queue: true }), undefined);
  assert.match(
    await control.sendInput("!pwd", [{ source: "path", path: "/repo/diagram.png" }]),
    /attachments/,
  );
  assert.ok(
    !machine.requests.some(({ method }) => method === "turn/start" || method === "turn/steer"),
  );
  assert.deepEqual(
    machine.requests.filter(({ method }) => method.endsWith("/shellCommand")),
    [
      { method: "thread/shellCommand", params: { threadId: "t1", command: "printf '%s' x" } },
      { method: "thread/queue/shellCommand", params: { threadId: "t1", command: "pwd" } },
    ],
  );
  control.release();
});

test("reasoning projection retains only public summary sections", () => {
  const item = {
    type: "reasoning",
    id: "r1",
    summary: ["**Public**\n\nFirst", "Second"],
    content: ["Raw content"],
  };
  assert.deepEqual(itemToEntry(item, 6, 1, "turn", "/root", true).values, [
    { kind: "reasoning", label: "Public", body: "**Public**\n\nFirst", live: false },
    { kind: "reasoning", label: undefined, body: "Second", live: true },
  ]);
  assert.deepEqual(itemToEntry({ ...item, summary: [] }, 6, 1, "turn").values, []);
});

test("editing waiting browser messages uses the native originals and leaves shell entries alone", async () => {
  const original = {
    id: "queued-id",
    kind: "message",
    input: [
      {
        type: "text",
        text: "[Image map.png]",
        text_elements: [{ placeholder: "[Image map.png]", byteRange: { start: 0, end: 15 } }],
      },
      { type: "image", url: "data:image/png;base64,AA==" },
    ],
    clientUserMessageId: "original-id",
    responsesapiClientMetadata: { client_type: "web" },
  };
  const shell = { id: "shell-id", kind: "shellCommand", input: [{ type: "text", text: "!pwd" }] };
  let waiting = [original, shell];
  const machine = fakeMachine({
    "thread/queue/list": () => ({ data: waiting, nextCursor: null }),
    "turn/steer": () => {
      throw new Error("The active turn changed.");
    },
    "thread/queue/take": () => {
      waiting = [shell];
      return { data: [original] };
    },
  });
  const control = new ThreadControl();
  control.connect("t1", machine);
  await Promise.resolve();
  await control.steerWaiting("active-turn");
  assert.deepEqual(
    machine.requests.filter((r) => ["turn/steer", "thread/queue/delete"].includes(r.method)),
    [
      {
        method: "turn/steer",
        params: {
          threadId: "t1",
          expectedTurnId: "active-turn",
          input: original.input,
          clientUserMessageId: original.clientUserMessageId,
          responsesapiClientMetadata: original.responsesapiClientMetadata,
        },
      },
    ],
  );
  await control.editWaiting();
  assert.deepEqual(machine.requests.find((r) => r.method === "thread/queue/take").params, {
    threadId: "t1",
    queuedSubmissionIds: [original.id],
  });
  assert.deepEqual(
    control.recoveries.map(({ input, queued, editing }) => ({ input, queued, editing })),
    [{ input: original.input, queued: true, editing: true }],
  );
  await control.recover(original.id, "retry", ["💡 [Image map.png]"]);
  assert.deepEqual(machine.requests.find((r) => r.method === "thread/queue/add").params, {
    threadId: "t1",
    clientUserMessageId: original.clientUserMessageId,
    responsesapiClientMetadata: original.responsesapiClientMetadata,
    input: [
      {
        type: "text",
        text: "💡 [Image map.png]",
        text_elements: [{ placeholder: "[Image map.png]", byteRange: { start: 5, end: 20 } }],
      },
      original.input[1],
    ],
  });
  assert.deepEqual(control.recoveries, []);
  assert.deepEqual(control.queued, [shell]);
  control.release();
});

test("a refused browser draft retains the same input and destination for explicit retry", async () => {
  let fail = true;
  const machine = fakeMachine({
    "thread/queue/add": () => {
      if (fail) throw new TransportError("Queue is full", "refused");
      return {};
    },
  });
  const control = new ThreadControl();
  control.connect("original-thread", machine);
  let transferred = false;
  assert.equal(
    await control.sendInput("Keep this", [{ source: "upload", name: "a.png", data: "AA==" }], {
      queue: true,
      onRecovery: () => {
        transferred = true;
      },
    }),
    "Queue is full",
  );
  assert.ok(transferred);
  const [recovery] = control.recoveries;
  control.connect("different-thread", machine);
  assert.deepEqual(control.recoveries, []);
  await control.recover(recovery.id, "retry");
  assert.equal(machine.requests.filter((r) => r.method === "thread/queue/add").length, 1);
  control.connect("original-thread", machine);
  fail = false;
  await control.recover(recovery.id, "retry");
  const [first, retry] = machine.requests.filter((r) => r.method === "thread/queue/add");
  assert.deepEqual(retry, first);
  assert.deepEqual(control.recoveries, []);
  control.release();
});

test("explicit skill choices follow edits and send the chosen catalog paths", async () => {
  const original = "$build and $build";
  const edited = "Please $build and $builder";
  const previous = [
    { start: 0, name: "build", path: "/first/SKILL.md" },
    { start: 11, name: "build", path: "/second/SKILL.md" },
  ];
  const shifted = "Please " + original;
  const skills = skillsAfterEdit(shifted, edited, skillsAfterEdit(original, shifted, previous));
  assert.deepEqual(skills, [{ ...previous[0], start: 7 }]);
  const machine = fakeMachine();
  const control = new ThreadControl();
  control.connect("t1", machine);
  assert.equal(await control.sendInput(edited, [], { skills }), undefined);
  const request = machine.requests.find(({ method }) => method === "turn/start");
  assert.deepEqual(request.params.input, [
    { type: "text", text: edited, text_elements: [] },
    { type: "skill", name: "build", path: "/first/SKILL.md" },
  ]);
  control.release();
});

test("shellGuard/requestApproval preserves argument boundaries with shell quoting", () => {
  const prompt = serverRequestToPrompt(
    {
      id: 21,
      method: "shellGuard/requestApproval",
      params: {
        threadId: "t1",
        turnId: "turn1",
        startedAtMs: 1000,
        executable: "rm",
        argv: ["-rf", "/tmp/a b", "it's", ""],
        cwd: "/repo",
      },
    },
    "t1",
  );
  assert.equal(prompt?.kind, "approval");
  assert.equal(prompt.approval.subject, "rm -rf '/tmp/a b' 'it'\\''s' ''");
});

test("approval without a native session decision remains one-time", () => {
  const machine = fakeMachine({ "thread/queue/list": () => ({ data: [], nextCursor: null }) });
  const control = new ThreadControl();
  control.connect("t1", machine);

  machine.emitServerRequest({
    id: 10,
    method: "item/commandExecution/requestApproval",
    params: {
      kind: "command",
      threadId: "t1",
      turnId: "turn1",
      itemId: "item1",
      startedAtMs: 1000,
      environmentId: null,
      command: "rm -rf /tmp/foo",
      cwd: "/repo",
    },
  });

  assert.equal(control.prompts.approvals.length, 1);
  const approval = control.prompts.approvals[0];
  assert.deepEqual(
    approval.choices.map((c) => c.decision),
    ["accept", "decline"],
  );

  const err = control.decideApproval(approval, 0);
  assert.equal(err, undefined);
  assert.deepEqual(machine.responses, [{ id: 10, result: { decision: "accept" } }]);
  assert.equal(control.prompts.approvals.length, 0);
  control.release();
});

test("WebUI offers and sends session approval only for supported requests", () => {
  const machine = fakeMachine({ "thread/queue/list": () => ({ data: [], nextCursor: null }) });
  const control = new ThreadControl();
  control.connect("t1", machine);

  const base = { threadId: "t1", turnId: "turn1", itemId: "item1", startedAtMs: 1000 };
  const cases = [
    [
      "item/commandExecution/requestApproval",
      { kind: "command", availableDecisions: ["acceptForSession"] },
      true,
    ],
    [
      "item/commandExecution/requestApproval",
      { kind: "humanCommand", availableDecisions: ["acceptForSession"] },
      false,
    ],
    [
      "item/commandExecution/requestApproval",
      {
        kind: "command",
        availableDecisions: ["acceptForSession"],
        networkApprovalContext: { host: "example.com", protocol: "https" },
      },
      false,
    ],
    ["item/fileChange/requestApproval", { grantRoot: null, reason: null }, true],
    ["item/fileChange/requestApproval", { grantRoot: "/workspace" }, false],
    [
      "shellGuard/requestApproval",
      { kind: "remoteWrite", executable: "git", argv: ["push"] },
      true,
    ],
    ["shellGuard/requestApproval", { kind: "sudo", executable: "sudo", argv: ["id"] }, false],
  ];
  for (const [index, [method, fields, canRemember]] of cases.entries()) {
    const id = index + 1;
    machine.emitServerRequest({ id, method, params: { ...base, ...fields } });
    const approval = control.prompts.approvals[0];
    if (!canRemember) {
      assert.deepEqual(
        approval.choices.map(({ decision }) => decision),
        ["accept", "decline"],
      );
      assert.equal(control.decideApproval(approval, 0), undefined);
      continue;
    }
    const remember = approval.choices.find(({ label }) => label === "Allow for session");
    assert.equal(control.decideApproval(approval, remember.id), undefined);
    assert.deepEqual(machine.responses.at(-1), {
      id,
      result:
        method === "shellGuard/requestApproval"
          ? { decision: "acceptForSession", reason: null }
          : { decision: "acceptForSession" },
    });
  }
  assert.equal(control.prompts.approvals.length, 0);
  control.release();
});

test("question answers send one-based choices, text without a choice, and skips", async () => {
  const machine = fakeMachine();
  const control = new ThreadControl();
  control.connect("t1", machine);
  const call = {
    id: "r1",
    requestId: "r1",
    threadId: "t1",
    blocking: true,
    questions: [
      { id: "choice", options: [{ label: "First option" }] },
      { id: "written", options: [{ label: "First option" }] },
      { id: "skipped" },
    ],
  };
  const answers = questionAnswers(call, {
    choice: { ...questionDraft(call.questions[0]), choice: 0, text: "old text", note: "A note" },
    written: {
      ...questionDraft(call.questions[1]),
      choice: 1,
      text: "My answer",
      note: "old note",
    },
    skipped: {
      ...questionDraft(call.questions[2]),
      choice: "skip",
      text: "old text",
      note: "old note",
    },
  });
  assert.equal(await control.answerQuestions(call, answers, AbortSignal.timeout(1000)), undefined);
  assert.deepEqual(machine.responses, [
    {
      id: "r1",
      result: {
        answers: [
          { id: "choice", choice: 1, note: "A note" },
          { id: "written", other: "My answer" },
          { id: "skipped", skipped: true },
        ],
      },
    },
  ]);
});

test("question answers limit the selected image drafts together and keep hidden images out", () => {
  const first = { id: "first", options: [{ label: "A" }] };
  const second = { id: "second" };
  const call = { blocking: true, questions: [first, second] };
  const firstImage = { id: "red", file: { size: 17 * 1024 * 1024 } };
  const secondImage = { id: "blue", file: { size: 16 * 1024 * 1024 } };
  const drafts = {
    first: { ...questionDraft(first), choice: 1, items: [firstImage] },
    second: { ...questionDraft(second), items: [secondImage] },
  };
  assert.throws(() => questionAnswers(call, drafts), /Uploads exceed the 32 MiB limit/);
  drafts.first.choice = 0;
  const [choice, other] = questionAnswers(call, drafts);
  assert.equal(choice.choice, 1);
  assert.equal(choice.items, undefined);
  assert.deepEqual(other.items, [secondImage]);
  drafts.first.choice = 1;
  secondImage.file.size -= 1024 * 1024;
  assert.deepEqual(
    questionAnswers(call, drafts).map((answer) => answer.items),
    [[firstImage], [secondImage]],
  );
});

test("secret user-input prompts are blocked from remote answering and shellGuard/requestSecret is ignored", async () => {
  const secretReq = serverRequestToPrompt(
    {
      id: 11,
      method: "shellGuard/requestSecret",
      params: { threadId: "t1", turnId: "turn1", startedAtMs: 1000 },
    },
    "t1",
  );
  assert.equal(secretReq, undefined);

  const machine = fakeMachine({ "thread/queue/list": () => ({ data: [], nextCursor: null }) });
  const control = new ThreadControl();
  control.connect("t1", machine);
  machine.emitServerRequest({
    id: 12,
    method: "item/tool/requestUserInput",
    params: {
      threadId: "t1",
      turnId: "turn1",
      itemId: "item1",
      isBlocking: true,
      autoResolutionMs: null,
      questions: [
        {
          id: "q1",
          header: "API key",
          question: "Enter secret key",
          isOther: true,
          isSecret: true,
          options: null,
        },
      ],
    },
  });
  assert.equal(control.prompts.questions.length, 1);
  assert.equal(control.prompts.questions[0].secretBlocked, true);
  const err = await control.answerQuestions(
    control.prompts.questions[0],
    [{ question: "q1", text: "super-secret" }],
    AbortSignal.timeout(1000),
  );
  assert.match(err, /locally/);
  assert.equal(machine.responses.length, 0);
  machine.emitServerRequest({
    id: 13,
    method: "shellGuard/requestSecret",
    params: { threadId: "t1", turnId: "turn1", startedAtMs: 1000 },
  });
  assert.equal(machine.errorResponses.length, 0);
  control.release();
});

test("unrecognized server requests return JSON-RPC -32601 while thread-mismatched prompts are ignored", () => {
  const machine = fakeMachine({ "thread/queue/list": () => ({ data: [], nextCursor: null }) });
  const control = new ThreadControl();
  control.connect("t1", machine);

  // Known prompt method for a different thread must be ignored, not rejected.
  machine.emitServerRequest({
    id: 21,
    method: "item/commandExecution/requestApproval",
    params: {
      threadId: "other-thread",
      turnId: "turn1",
      itemId: "item1",
      command: "ls",
    },
  });
  assert.equal(control.prompts.approvals.length, 0);
  assert.equal(machine.errorResponses.length, 0);

  // Terminal-handled permission prompts must be ignored, not auto-denied.
  machine.emitServerRequest({
    id: 215,
    method: "item/permissions/requestApproval",
    params: {
      threadId: "t1",
      turnId: "turn1",
      itemId: "item1",
    },
  });
  assert.equal(control.prompts.approvals.length, 0);
  assert.equal(machine.errorResponses.length, 0);

  // Unrecognized server request method must be rejected with -32601.
  machine.emitServerRequest({
    id: 22,
    method: "unknown/unsupportedMethod",
    params: { threadId: "t1" },
  });
  assert.equal(control.prompts.approvals.length, 0);
  assert.equal(control.prompts.questions.length, 0);
  assert.deepEqual(machine.errorResponses, [
    {
      id: 22,
      code: -32601,
      message: "unsupported web client server request: unknown/unsupportedMethod",
    },
  ]);
  control.release();
});

test("Stream hydrates via thread/resume and applies live ServerNotification deltas", async () => {
  const machine = fakeMachine({
    "thread/resume": () => ({
      thread: {
        id: "t1",
        sessionId: "s1",
        forkedFromId: null,
        parentThreadId: null,
        preview: "Hello",
        ephemeral: false,
        section: null,
        sectionEnteredAt: null,
        projectId: null,
        historyMode: "full",
        modelProvider: "openai",
        model: "gpt-5",
        reasoningEffort: "medium",
        serviceTier: null,
        contextTokens: 100,
        contextWindow: 128000,
        completedCompactions: 0,
        agentStatus: null,
        createdAt: 1700000000,
        updatedAt: 1700000000,
        recencyAt: null,
        status: { type: "idle" },
        path: null,
        cwd: "/repo",
        cliVersion: "0.1",
        originator: null,
        source: "cli",
        threadSource: null,
        agentNickname: null,
        agentRole: null,
        gitInfo: null,
        name: "My Thread",
        turns: [],
      },
      model: "gpt-5",
      modelProvider: "openai",
      serviceTier: null,
      disabledPluginIds: [],
      cwd: "/repo",
      instructionSources: [],
      approvalPolicy: "on-request",
      approvalsReviewer: "user",
      sandbox: { type: "workspaceWrite" },
      reasoningEffort: "medium",
      collaborationMode: null,
      turnsBackwardsCursor: null,
      itemsBackwardsCursor: null,
    }),
    "thread/subtree/list": () => ({ data: [], nextCursor: null, backwardsCursor: null }),
    "thread/changes/read": () => ({
      threadId: "t1",
      workspace: "/repo",
      changes: [],
      files: [],
      totalAdditions: 0,
      totalDeletions: 0,
    }),
  });

  const s = new Stream();
  s.connect("t1", machine);
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(s.session?.title, "My Thread");
  assert.equal(s.session?.model, "gpt-5");

  machine.emitNotification({
    method: "turn/started",
    params: {
      threadId: "t1",
      turn: {
        id: "turn-1",
        rootTurnId: null,
        items: [],
        itemsView: "full",
        status: "inProgress",
        error: null,
        startedAt: 1700000001,
        completedAt: null,
        durationMs: null,
      },
    },
  });
  assert.equal(s.activeTurnId, "turn-1");

  machine.emitNotification({
    method: "item/started",
    params: {
      threadId: "t1",
      turnId: "turn-1",
      startedAtMs: 1700000001000,
      item: {
        type: "agentMessage",
        id: "msg-1",
        text: "Hel",
        moderation: null,
        phase: null,
        memoryCitation: null,
        delivery: null,
        questions: null,
      },
    },
  });
  const structure = s.main;
  machine.emitNotification({
    method: "item/agentMessage/delta",
    params: { threadId: "t1", turnId: "turn-1", itemId: "msg-1", delta: "lo!" },
  });
  assert.equal(s.main, structure);
  assert.equal(s.entry(1).text, "Hello!");

  machine.emitNotification({
    method: "thread/started",
    params: {
      thread: {
        id: "t-child",
        sessionId: "s2",
        forkedFromId: null,
        parentThreadId: "t1",
        preview: "Child task",
        ephemeral: true,
        section: null,
        sectionEnteredAt: null,
        projectId: null,
        historyMode: "full",
        modelProvider: "openai",
        model: "gpt-5",
        reasoningEffort: "medium",
        serviceTier: null,
        contextTokens: 0,
        contextWindow: 128000,
        completedCompactions: 0,
        agentStatus: null,
        createdAt: 1700000002,
        updatedAt: 1700000002,
        recencyAt: null,
        status: { type: "active", activeFlags: [] },
        path: null,
        cwd: "/repo",
        cliVersion: "0.1",
        originator: null,
        source: "subagent",
        threadSource: null,
        agentNickname: "worker",
        agentRole: "worker",
        gitInfo: null,
        name: "worker",
        turns: [],
      },
    },
  });
  assert.ok(s.roster.agents.some((a) => a.thread === "t-child"));

  s.close();
  assert.ok(
    machine.requests.some((r) => r.method === "thread/unsubscribe" && r.params.threadId === "t1"),
  );
});

test("warm thread selection retains history, receives offscreen events and resumes only after reconnect", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const machine = fakeMachine({
    "thread/resume": ({ threadId }) => ({
      thread: { id: threadId, cwd: "/repo", status: { type: "idle" }, turns: [] },
      initialTurnsPage: {
        data: [{ id: "recent", items: [{ id: "item", type: "agentMessage", text: threadId }] }],
        nextCursor: "older",
      },
    }),
    "thread/turns/list": ({ threadId }) => ({
      data: [
        { id: "old", items: [{ id: "item", type: "agentMessage", text: `${threadId} older` }] },
      ],
    }),
  });
  const count = (method, id) =>
    machine.requests.filter((entry) => entry.method === method && entry.params.threadId === id)
      .length;
  const threads = new ThreadStreams();
  threads.connect("a", machine);
  const a = threads.current;
  await a.refresh();
  assert.equal(await a.earlier(), true);
  const retained = a.entry(1);

  threads.connect("b", machine);
  const b = threads.current;
  await b.refresh();
  machine.emitNotification({
    method: "item/agentMessage/delta",
    params: { threadId: "a", turnId: "recent", itemId: "item", delta: " streamed" },
  });
  assert.equal(b.entry(1).text, "b");
  threads.connect("a", machine);
  assert.equal(threads.current, a);
  assert.equal(a.entry(1), retained);
  assert.equal(a.entry(2).text, "a streamed");
  assert.equal(count("thread/resume", "a"), 1);
  assert.equal(count("thread/unsubscribe", "a"), 0);

  machine.emitOpen();
  await a.refresh();
  assert.equal(count("thread/resume", "a"), 2);
  assert.equal(count("thread/resume", "b"), 1);
  assert.equal(a.entry(1).text, "a older");
  assert.equal(a.hasEarlier, false);

  t.mock.timers.tick(5 * 60_000);
  assert.equal(b.threadId, undefined);
  assert.equal(threads.current, a);
  assert.equal(count("thread/unsubscribe", "a"), 0);
  threads.release("a", machine);
  t.mock.timers.tick(5 * 60_000);
  assert.equal(count("thread/unsubscribe", "a"), 1);
});

test("eviction orders a pending resume, unsubscribe, and subsequent reopen of the same thread", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const firstResume = Promise.withResolvers();
  const evicting = Promise.withResolvers();
  const unsubscribe = Promise.withResolvers();
  let aResumes = 0;
  const machine = fakeMachine({
    "thread/resume": ({ threadId }) => {
      if (threadId === "a" && ++aResumes === 1) return firstResume.promise;
      return resumedThread(threadId);
    },
    "thread/unsubscribe": ({ threadId }) => {
      if (threadId !== "a") return {};
      evicting.resolve();
      return unsubscribe.promise;
    },
  });
  const threads = new ThreadStreams();
  threads.connect("a", machine);
  for (const id of ["b", "c", "d", "e", "f", "g"]) threads.connect(id, machine);
  threads.connect("a", machine);
  assert.equal(aResumes, 1);
  assert.ok(
    !machine.requests.some(
      (req) => req.method === "thread/unsubscribe" && req.params.threadId === "a",
    ),
  );

  firstResume.resolve(resumedThread("a"));
  await evicting.promise;
  assert.equal(aResumes, 1);
  unsubscribe.resolve({ status: "unsubscribed" });
  await threads.current.refresh();
  assert.equal(aResumes, 2);
  assert.equal(threads.current.session.thread, "a");
});

test("reconnect joins earlier history, then applies live deltas and keeps the fresh terminal list", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const root = resumedThread("a").thread;
  const child = { ...root, id: "b", parentThreadId: root.id };
  let processId = "before-reconnect";
  let failTerminalRead = false;
  let latest = transcriptTurnPage(10, 6, "after6");
  const gapPageRequested = Promise.withResolvers();
  const gapPage = Promise.withResolvers();
  const machine = fakeMachine({
    "thread/resume": ({ threadId }) =>
      threadId === root.id ? resumedThread(root.id, latest) : { thread: child },
    "thread/subtree/list": () => ({ data: [root, child] }),
    "thread/backgroundTerminals/list": ({ threadId }) => {
      if (threadId !== root.id) return { data: [] };
      if (failTerminalRead) {
        failTerminalRead = false;
        throw new Error("Terminal list temporarily unavailable");
      }
      return { data: [{ processId }] };
    },
    "thread/turns/list": ({ cursor }) => {
      if (cursor === "after6") return transcriptTurnPage(5, 1);
      if (cursor === "after33") return transcriptTurnPage(32, 13, "after13");
      assert.equal(cursor, "after13");
      gapPageRequested.resolve();
      return gapPage.promise;
    },
  });
  const threads = new ThreadStreams();
  threads.connect(root.id, machine);
  const stream = threads.current;
  await stream.refresh();
  assert.equal(await stream.earlier(), true);
  threads.connect(child.id, machine);
  await threads.current.refresh();
  threads.connect(root.id, machine);
  const visibleText = () => stream.transcriptEntries.map(({ seq }) => stream.entry(seq).text);
  const retained = visibleText();
  const notifyTerminalChange = () =>
    machine.emitNotification({
      method: "thread/backgroundTerminals/changed",
      params: { threadId: root.id },
    });

  latest = transcriptTurnPage(37, 33, "after33");
  machine.emitOpen();
  await gapPageRequested.promise;
  processId = "during-catch-up";
  notifyTerminalChange();
  await new Promise(setImmediate);
  assert.deepEqual(stream.processes, [{ processId, threadId: root.id }]);

  processId = "after-reconnect";
  failTerminalRead = true;
  notifyTerminalChange();
  machine.emitNotification({
    method: "item/agentMessage/delta",
    params: { threadId: "a", turnId: "t37", itemId: "m37", delta: " streamed" },
  });
  assert.deepEqual(visibleText(), retained);
  gapPage.resolve(transcriptTurnPage(12, 1));
  await stream.refresh();

  const expected = Array.from({ length: 36 }, (_, index) => `turn ${index + 1}`);
  assert.deepEqual(visibleText(), [...expected, "turn 37 streamed"]);
  assert.equal(stream.hasEarlier, false);
  await new Promise(setImmediate);
  assert.deepEqual(stream.processes, [{ processId: "after-reconnect", threadId: root.id }]);
  await stream.close();
});

test("a tool-heavy turn reveals older retained items without requesting a native page", async () => {
  const items = Array.from({ length: 90 }, (_, index) => ({
    id: `m${index + 1}`,
    type: "agentMessage",
    text: `item ${index + 1}`,
  }));
  const machine = fakeMachine({
    "thread/resume": () => resumedThread("a", { data: [{ id: "t", items }] }),
  });
  const stream = new Stream();
  stream.connect("a", machine);
  await stream.refresh();
  const shown = () => stream.transcriptEntries.map(({ itemId }) => itemId);
  assert.deepEqual(
    shown(),
    items.slice(-40).map(({ id }) => id),
  );
  assert.equal(stream.hasEarlier, true);
  assert.equal(await stream.earlier(), true);
  assert.deepEqual(
    shown(),
    items.slice(-80).map(({ id }) => id),
  );
  assert.equal(await stream.earlier(), true);
  assert.deepEqual(
    shown(),
    items.map(({ id }) => id),
  );
  assert.equal(stream.hasEarlier, false);
  assert.equal(await stream.earlier(), false);
  assert.ok(!machine.requests.some(({ method }) => method === "thread/turns/list"));
  await stream.close();
});

test("live edits follow patch notifications independently of saved changes", async () => {
  let finishRead;
  const machine = fakeMachine({
    "thread/resume": () => ({
      thread: { id: "t1", cwd: "/repo", status: { type: "idle" }, turns: [] },
    }),
    "thread/subtree/list": () => ({ data: [] }),
    "thread/changes/read": ({ itemId }) =>
      itemId === "patch-1"
        ? new Promise((resolve) => (finishRead = resolve))
        : {
            workspace: "/repo",
            changes: [],
            files: [
              {
                path: "saved.txt",
                status: "modified",
                additions: 1,
                deletions: 0,
                unifiedDiff: "+saved",
              },
            ],
            totalAdditions: 1,
            totalDeletions: 0,
          },
  });
  const stream = new Stream();
  stream.connect("t1", machine);
  await stream.refresh();
  const turn = { id: "turn-1", status: "inProgress", itemsView: "full", items: [] };
  machine.emitNotification({ method: "turn/started", params: { threadId: "t1", turn } });
  const patch = (threadId, text) =>
    machine.emitNotification({
      method: "item/fileChange/patchUpdated",
      params: {
        threadId,
        turnId: turn.id,
        itemId: "patch-1",
        changes: [{ path: "live.txt", kind: { type: "add" }, diff: text }],
      },
    });
  patch("other", "+ignored");
  assert.deepEqual(stream.live, []);
  patch("t1", "first\n");
  patch("t1", "latest\n");
  assert.deepEqual(
    stream.live.map((preview) => preview.Files[0].Diff),
    ["@@ -0 +1 @@\n+latest"],
  );
  assert.deepEqual(
    stream.review.files.map((file) => file.path),
    ["saved.txt"],
  );
  machine.emitNotification({
    method: "item/completed",
    params: {
      threadId: "t1",
      turnId: turn.id,
      item: { id: "patch-1", type: "fileChange", status: "completed", changes: [] },
    },
  });
  patch("t1", "after patch finished\n");
  assert.equal(stream.live[0].Settling, true);
  assert.equal(stream.live[0].SavedPath, undefined);
  assert.match(stream.live[0].Files[0].Diff, /latest/);
  finishRead({
    changes: [{ threadId: "t1", itemId: "patch-1", turnId: turn.id }],
    files: [{ absolutePath: "/repo/live.txt", unifiedDiff: "@@ -0 +1 @@\n+retained" }],
  });
  await new Promise(setImmediate);
  assert.equal(stream.live[0].Complete, true);
  assert.equal(stream.live[0].SavedPath, "/repo/live.txt");
  assert.match(stream.live[0].Files[0].Diff, /retained/);
  machine.emitNotification({
    method: "turn/completed",
    params: { threadId: "t1", turn: { ...turn, status: "completed" } },
  });
  patch("t1", "+cached too late");
  assert.match(stream.live[0].Files[0].Diff, /retained/);
  stream.close();
});

test("background-terminal refresh clears only the thread reported unloaded", async () => {
  const root = { id: "t1", cwd: "/repo", status: { type: "idle" }, turns: [] };
  const child = { ...root, id: "t2", parentThreadId: root.id };
  let refusal;
  const machine = fakeMachine({
    "thread/resume": () => ({ thread: root }),
    "thread/subtree/list": () => ({ data: [root, child] }),
    "thread/backgroundTerminals/list": ({ threadId }) => {
      if (threadId === child.id && refusal) throw refusal;
      return { data: [{ processId: `process-${threadId}` }] };
    },
  });
  const stream = new Stream();
  stream.connect(root.id, machine);
  await new Promise(setImmediate);
  const processIds = () => stream.processes.map(({ processId }) => processId);
  assert.deepEqual(processIds(), ["process-t1", "process-t2"]);

  const refreshChild = async () => {
    machine.emitNotification({
      method: "thread/backgroundTerminals/changed",
      params: { threadId: child.id },
    });
    await new Promise(setImmediate);
  };
  const missingThread = (threadId) => {
    const message = "That thread has gone away.";
    return new TransportError(message, "refused", undefined, {
      code: -32600,
      message,
      data: { reason: "threadNotLoaded", threadId },
    });
  };

  refusal = new TransportError(`thread not found: ${child.id}`, "refused");
  await refreshChild();
  assert.deepEqual(processIds(), ["process-t1", "process-t2"]);

  refusal = missingThread(root.id);
  await refreshChild();
  assert.deepEqual(processIds(), ["process-t1", "process-t2"]);

  refusal = missingThread(child.id);
  await refreshChild();
  assert.deepEqual(processIds(), ["process-t1"]);
  stream.close();
});

test("one terminal change refreshes both warm family views with one read", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const root = { id: "root", cwd: "/repo", status: { type: "idle" }, turns: [] };
  const child = { ...root, id: "child", parentThreadId: root.id };
  let terminals = [];
  const machine = fakeMachine({
    "thread/resume": ({ threadId }) => ({ thread: threadId === root.id ? root : child }),
    "thread/subtree/list": () => ({ data: [root, child] }),
    "thread/backgroundTerminals/list": ({ threadId }) => ({
      data: threadId === child.id ? terminals : [],
    }),
  });
  const threads = new ThreadStreams();
  threads.connect(root.id, machine);
  const rootView = threads.current;
  await rootView.refresh();
  threads.connect(child.id, machine);
  const childView = threads.current;
  await childView.refresh();
  await new Promise(setImmediate);
  const before = machine.requests.length;

  terminals = [{ processId: "live" }];
  machine.emitNotification({
    method: "thread/backgroundTerminals/changed",
    params: { threadId: child.id },
  });
  await new Promise(setImmediate);

  assert.deepEqual(machine.requests.slice(before), [
    { method: "thread/backgroundTerminals/list", params: { threadId: child.id } },
  ]);
  assert.deepEqual(rootView.processes, [{ processId: "live", threadId: child.id }]);
  assert.deepEqual(childView.processes, rootView.processes);
});

test("saved edits page, retain exact caller provenance, and discard stale pages", async () => {
  const root = { id: "t1", cwd: "/repo", status: { type: "idle" }, turns: [] };
  const child = {
    ...root,
    id: "t2",
    parentThreadId: "t1",
    agentNickname: "Pasteur",
    source: {
      subAgent: {
        thread_spawn: {
          parent_thread_id: "t1",
          depth: 1,
          agent_path: "/root/worker",
          agent_nickname: "Pasteur",
          agent_role: "worker",
        },
      },
    },
  };
  const edit = {
    changeId: "apple1",
    authorThreadId: "t2",
    threadId: "t2",
    sourceKind: "ShellWrite",
    sourceLabel: "Shell",
  };
  const workerFile = {
    path: "shared.txt",
    absolutePath: "/repo/shared.txt",
    changeIds: ["apple1"],
    status: "M",
    unifiedDiff: "@@ -1 +1 @@\n-old\n+new\n",
    additions: 1,
    deletions: 1,
  };
  const historicalFile = { ...workerFile, absolutePath: "/outside/shared.txt", changeIds: [] };
  const page = {
    files: [workerFile],
    workspace: "/repo",
    changes: [edit],
    fileCount: 2,
    nextCursor: "23:1",
    totalAdditions: 2,
    totalDeletions: 1,
    callers: [{ threadId: "t2", fileCount: 1, additions: 1, deletions: 1 }],
  };
  let next;
  const machine = fakeMachine({
    "thread/resume": ({ threadId }) => ({ thread: threadId === root.id ? root : child }),
    "thread/subtree/list": () => ({ data: [root, child] }),
    "thread/changes/read": ({ callerThreadId, cursor }) => {
      if (cursor) return new Promise((resolve) => (next = resolve));
      if (callerThreadId === "t2")
        return { ...page, fileCount: 1, totalAdditions: 1, nextCursor: null };
      return page;
    },
  });
  const stream = new Stream();
  stream.connect(root.id, machine);
  await stream.refresh();
  assert.deepEqual(stream.review.files[0].origins, [
    { Change: "apple1", Caller: "/root/worker", Source: "Shell" },
  ]);
  assert.equal(stream.label("t2"), "worker");
  assert.equal(stream.label("/root/agent_a"), "agent_a");
  assert.deepEqual(stream.reviewOverview.callers[0].counts, { Added: 1, Removed: 1 });
  assert.deepEqual(stream.reviewOverview.counts.Callers, { t2: { Added: 1, Removed: 1 } });
  let pending = stream.moreChanges();
  next({ ...page, files: [historicalFile], nextCursor: null });
  await pending;
  assert.deepEqual(
    stream.review.files.map((file) => file.path),
    ["/repo/shared.txt", "/outside/shared.txt"],
  );
  await stream.refresh();
  pending = stream.moreChanges();
  stream.selectChangeCaller("t2");
  next({ ...page, files: [historicalFile], nextCursor: null });
  await pending;
  assert.equal(stream.reviewCaller, "t2");
  assert.equal(stream.review.fileCount, 1);
  assert.equal(stream.review.files.length, 1);
  assert.ok(
    machine.requests.some(
      ({ method, params }) =>
        method === "thread/changes/read" &&
        params.threadId === "t1" &&
        params.cursor === "23:1" &&
        params.limit === 20,
    ),
  );
  assert.ok(
    machine.requests.some(
      ({ method, params }) => method === "thread/changes/read" && params.callerThreadId === "t2",
    ),
  );
  stream.close();
});

test("Main recovers a late delegate's shell edits and follows exact command completion", async () => {
  const main = { id: "t1", cwd: "/main", status: { type: "idle" }, turns: [] };
  const worker = {
    ...main,
    id: "t2",
    parentThreadId: "t1",
    cwd: "/worker",
    agentNickname: "writer",
  };
  const empty = { changes: [], files: [] };
  let savedDuringAttach = false;
  const sample = {
    threadId: worker.id,
    turnId: "turn-1",
    itemId: "shell-1",
    command: "bash -c 'printf live > out.txt'",
    source: "unifiedExecStartup",
    changes: [{ path: "/worker/out.txt", kind: { type: "add" }, diff: "@@ -0 +1 @@\n+live" }],
  };
  const machine = fakeMachine({
    "thread/resume": () => ({ thread: main }),
    "thread/subtree/list": () => ({ data: [main] }),
    "thread/changes/read": ({ threadId, itemId }) => {
      if (threadId === worker.id && itemId === sample.itemId)
        return {
          changes: [{ threadId, itemId, turnId: sample.turnId }],
          files: [{ absolutePath: "/worker/out.txt", unifiedDiff: "@@ -0 +1 @@\n+saved" }],
        };
      if (threadId === main.id && !itemId && savedDuringAttach)
        return { ...empty, files: [{ absolutePath: "/worker/finished.txt" }] };
      return empty;
    },
  });
  const stream = new Stream();
  stream.connect(main.id, machine);
  await stream.refresh();
  const notify = (status, turnId = sample.turnId, itemId = sample.itemId) =>
    machine.emitNotification({
      method: "item/completed",
      params: {
        threadId: worker.id,
        turnId,
        item: {
          type: "commandExecution",
          id: itemId,
          command: sample.command,
          source: sample.source,
          commandActions: [{ type: "unknown" }],
          status,
        },
      },
    });

  machine.emitNotification({ method: "item/commandExecution/changesUpdated", params: sample });
  assert.deepEqual(stream.live, []);
  machine.emitNotification({ method: "thread/started", params: { thread: worker } });
  savedDuringAttach = true;
  notify("completed", sample.turnId, "already-finished");
  machine.emitNotification({ method: "item/commandExecution/changesUpdated", params: sample });
  await new Promise(setImmediate);
  assert.equal(stream.review.files[0].path, "/worker/finished.txt");
  assert.equal(stream.live.length, 1);
  assert.equal(stream.live[0].Caller, "writer");
  assert.equal(stream.live[0].Workspace, "/worker");
  assert.match(stream.live[0].Files[0].Diff, /\+live/);
  machine.emitNotification({
    method: "turn/completed",
    params: { threadId: worker.id, turn: { id: sample.turnId, status: "completed" } },
  });
  machine.emitNotification({
    method: "turn/started",
    params: { threadId: worker.id, turn: { id: "turn-2", status: "inProgress" } },
  });
  machine.emitNotification({
    method: "item/commandExecution/changesUpdated",
    params: { ...sample, changes: [{ ...sample.changes[0], diff: "@@ -0 +1 @@\n+late" }] },
  });
  notify("completed", "turn-2");
  notify("inProgress");
  assert.equal(stream.live[0].Complete, undefined);
  assert.equal(stream.live[0].Settling, undefined);
  assert.match(stream.live[0].Files[0].Diff, /\+late/);
  notify("failed");
  assert.equal(stream.live[0].Settling, true);
  await new Promise(setImmediate);
  assert.equal(stream.live[0].SavedPath, "/worker/out.txt");
  assert.match(stream.live[0].Files[0].Diff, /\+saved/);
  stream.close();
});

test("shell preview settlement distinguishes missing from failed and empty patchUpdated withdraws", async () => {
  const main = { id: "t1", cwd: "/repo", status: { type: "idle" }, turns: [] };
  const empty = { changes: [], files: [] };
  const sample = {
    threadId: main.id,
    turnId: "turn-1",
    itemId: "shell-without-capture",
    command: "printf live > out.txt",
    source: "userShell",
    changes: [{ path: "/repo/out.txt", kind: { type: "add" }, diff: "@@ -0 +1 @@\n+attach" }],
  };
  const failed = { ...sample, itemId: "shell-read-failed" };
  const machine = fakeMachine({
    "thread/resume": () => ({ thread: main }),
    "thread/subtree/list": () => ({ data: [main] }),
    "thread/changes/read": ({ itemId }) => {
      if (itemId === failed.itemId) throw new Error("capture store unavailable");
      return empty;
    },
  });
  const stream = new Stream();
  stream.connect(main.id, machine);
  await new Promise(setImmediate);
  machine.emitNotification({ method: "item/commandExecution/changesUpdated", params: sample });
  machine.emitNotification({ method: "item/commandExecution/changesUpdated", params: failed });
  assert.equal(stream.live.length, 2);
  assert.ok(stream.live.every((edit) => edit.Caller === "User shell"));

  machine.emitNotification({
    method: "item/commandExecution/changesUpdated",
    params: { ...sample, changes: [{ ...sample.changes[0], diff: "@@ -0 +1 @@\n+push" }] },
  });
  const find = ({ itemId }) => stream.live.find((edit) => edit.ID.includes(itemId));
  assert.match(find(sample).Files[0].Diff, /\+push/);

  machine.emitNotification({
    method: "turn/started",
    params: { threadId: main.id, turn: { id: "turn-1", status: "inProgress", items: [] } },
  });
  machine.emitNotification({
    method: "item/fileChange/patchUpdated",
    params: {
      threadId: main.id,
      turnId: "turn-1",
      itemId: "retracted-call",
      changes: [{ path: "/repo/tmp.txt", kind: { type: "add" }, diff: "hello\n" }],
    },
  });
  assert.equal(find({ itemId: "retracted-call" })?.Files.length, 1);
  machine.emitNotification({
    method: "item/fileChange/patchUpdated",
    params: {
      threadId: main.id,
      turnId: "turn-1",
      itemId: "retracted-call",
      changes: [],
    },
  });
  assert.equal(find({ itemId: "retracted-call" })?.Withdrawn, true);
  assert.deepEqual(find({ itemId: "retracted-call" })?.Files, []);

  for (const { threadId, turnId, itemId } of [sample, failed]) {
    machine.emitNotification({
      method: "item/completed",
      params: {
        threadId,
        turnId,
        item: {
          id: itemId,
          type: "commandExecution",
          status: "completed",
          command: sample.command,
        },
      },
    });
  }
  await new Promise(setImmediate);
  assert.equal(find(sample).Complete, true);
  assert.equal(find(sample).SavedPath, undefined);
  assert.deepEqual(find(sample).Files, []);
  assert.equal(find(failed).Complete, true);
  assert.equal(find(failed).SavedPath, undefined);
  assert.match(find(failed).Files[0].Diff, /\+attach/);
  assert.match(find(failed).Status, /Saved Diff could not be read/);
  stream.close();
});

test("edit receipts follow visible transcript pages and publish before the final capture page", async () => {
  const [older, recent, live] = ["older", "recent", "live"].map((id) => ({
    id,
    type: "commandExecution",
    command: "make",
    status: "completed",
  }));
  const turn = {
    id: "turn",
    status: "completed",
    items: [
      older,
      ...Array.from({ length: 39 }, (_, i) => ({
        id: `m${i}`,
        type: "agentMessage",
        text: "filler",
      })),
      recent,
    ],
  };
  const threadId = "receipts";
  const continuation = Promise.withResolvers();
  const page = (itemId, path, sequence, nextCursor) => ({
    nextCursor,
    changes: [{ threadId, turnId: turn.id, itemId, changeId: path, sequence }],
    files: [{ path, status: "A", changeIds: [path], scope: "workspace", additions: 2 }],
  });
  const machine = fakeMachine({
    "thread/resume": () => resumedThread(threadId, { data: [turn] }),
    "thread/subtree/list": () => ({ data: [] }),
    "thread/changes/read": ({ receiptItemIds, cursor }) => {
      if (!receiptItemIds) return { files: [], changes: [] };
      assert.equal(receiptItemIds.length, 1);
      const [itemId] = receiptItemIds;
      if (cursor) return continuation.promise;
      return page(itemId, `${itemId}.txt`, 2, itemId === recent.id ? "2:1" : undefined);
    },
  });
  const stream = new Stream();
  stream.connect(threadId, machine);
  await stream.refresh();
  const paths = (itemId) => {
    const seed = stream.transcriptEntries.find((entry) => entry.itemId === itemId);
    return (
      seed &&
      stream
        .entry(seed.seq)
        .values.filter((value) => value.effect)
        .map((value) => value.path)
    );
  };
  const requested = () =>
    machine.requests.map(({ params }) => params?.receiptItemIds).filter(Boolean);
  assert.deepEqual(requested(), [[recent.id], [recent.id]]);
  assert.deepEqual(paths(recent.id), ["recent.txt"]);
  continuation.resolve(page(recent.id, "first.txt", 1));
  assert.equal(await stream.earlier(), true);
  await new Promise(setImmediate);
  assert.deepEqual(paths(recent.id), ["first.txt", "recent.txt"]);
  assert.deepEqual(paths(older.id), ["older.txt"]);
  machine.emitNotification({
    method: "item/completed",
    params: { threadId, turnId: turn.id, item: live },
  });
  await new Promise(setImmediate);
  assert.deepEqual(paths(live.id), ["live.txt"]);
  assert.deepEqual(requested(), [[recent.id], [recent.id], [older.id], [live.id]]);
  stream.close();
});

test("itemToEntry maps commandExecution with bash lang and fileChange as per-file op rows", () => {
  const cmdEntry = itemToEntry(
    {
      id: "c1",
      type: "commandExecution",
      command: "/bin/bash -lc 'git diff --stat'",
      display: { script: "git diff --stat", segments: [], effectiveCwd: null },
      cwd: "/repo",
      status: "completed",
      exitCode: 0,
      durationMs: 42,
      aggregatedOutput: "a.ts | 2 +-\n",
    },
    1,
    "turn-1",
    "/root",
  );
  assert.equal(cmdEntry.kind, "tool");
  assert.equal(cmdEntry.values?.[0]?.kind, "op");
  assert.equal(cmdEntry.values?.[0]?.verb, "Ran");
  assert.equal(cmdEntry.values?.[0]?.lang, "bash");
  assert.equal(cmdEntry.values?.[0]?.code, "git diff --stat");

  const editEntry = itemToEntry(
    {
      id: "f1",
      type: "fileChange",
      status: "completed",
      changes: [
        { path: "a.ts", kind: { type: "add" }, diff: "+line1\n+line2\n" },
        {
          path: "b.ts",
          kind: { type: "update", move_path: "c.ts" },
          diff: "@@ -1 +1 @@\n-old\n+new\n",
        },
        { path: "d.ts", kind: { type: "delete" }, diff: "-gone\n" },
      ],
    },
    2,
    "turn-1",
    "/root",
  );
  assert.equal(editEntry.values?.length, 3);
  assert.equal(editEntry.values?.[0]?.verb, "Create");
  assert.equal(editEntry.values?.[0]?.label, "+2 −0");
  assert.equal(editEntry.values?.[0]?.lang, "diff");
  assert.equal(editEntry.values?.[1]?.verb, "Move");
  assert.equal(editEntry.values?.[1]?.path, "b.ts => c.ts");
  assert.equal(editEntry.values?.[1]?.label, "+1 −1");
  assert.equal(editEntry.values?.[2]?.verb, "Delete");
  assert.equal(editEntry.values?.[2]?.label, "+0 −1");
});

test("turn/completed with summary itemsView preserves existing turn items and tracks cost & TPS", async () => {
  const machine = fakeMachine({
    "thread/resume": () => ({
      thread: {
        id: "t-summary",
        preview: "hi",
        cwd: "/repo",
        createdAt: 1700000000,
        status: { type: "idle" },
        turns: [],
      },
      model: "gpt-6.2-sol",
      reasoningEffort: "medium",
      serviceTier: null,
    }),
    "thread/subtree/list": () => ({ data: [] }),
    "thread/changes/read": () => ({
      threadId: "t-summary",
      workspace: "/repo",
      totalAdditions: 0,
      totalDeletions: 0,
      changes: [],
      files: [],
    }),
  });
  const s = new Stream();
  s.connect("t-summary", machine);
  await new Promise((r) => setTimeout(r, 15));

  machine.emitNotification({
    method: "turn/started",
    params: {
      threadId: "t-summary",
      turn: { id: "turn-1", status: "inProgress", items: [], itemsView: "full", error: null },
    },
  });
  machine.emitNotification({
    method: "item/completed",
    params: {
      threadId: "t-summary",
      turnId: "turn-1",
      item: { id: "u1", type: "userMessage", content: [{ type: "text", text: "Run pwd" }] },
    },
  });
  machine.emitNotification({
    method: "item/completed",
    params: {
      threadId: "t-summary",
      turnId: "turn-1",
      item: {
        id: "c1",
        type: "commandExecution",
        command: "/bin/bash -lc pwd",
        display: { script: "pwd", segments: [], effectiveCwd: null },
        cwd: "/repo",
        status: "completed",
        exitCode: 0,
        aggregatedOutput: "/repo\n",
      },
    },
  });
  await new Promise((r) => setTimeout(r, 10));
  machine.emitNotification({
    method: "thread/tokenUsage/updated",
    params: {
      threadId: "t-summary",
      tokenUsage: {
        total: {
          totalTokens: 1500,
          inputTokens: 1000,
          cachedInputTokens: 200,
          cacheWriteInputTokens: 0,
          outputTokens: 500,
          reasoningOutputTokens: 0,
        },
        last: {
          totalTokens: 1500,
          inputTokens: 1000,
          cachedInputTokens: 200,
          cacheWriteInputTokens: 0,
          outputTokens: 500,
          reasoningOutputTokens: 0,
        },
        modelContextWindow: 272000,
        estimatedCostUsd: 0.00662,
        costPartial: false,
        outputTokensPerSecond: 42.5,
        workDurationMs: 1200,
        roundtrips: 2,
      },
    },
  });
  machine.emitNotification({
    method: "turn/completed",
    params: {
      threadId: "t-summary",
      turn: {
        id: "turn-1",
        status: "completed",
        itemsView: "summary",
        items: [{ id: "a1", type: "agentMessage", text: "Done." }],
        durationMs: 1200,
        error: null,
      },
    },
  });

  assert.equal(s.main.order.length, 3);
  assert.equal(s.main.entries[s.main.order[0]].kind, "user");
  assert.equal(s.main.entries[s.main.order[1]].kind, "tool");
  assert.equal(s.main.entries[s.main.order[2]].kind, "assistant");

  machine.emitNotification({
    method: "thread/skills/updated",
    params: { threadId: "t-summary", names: ["terminal", "codex", "terminal"] },
  });
  machine.emitNotification({
    method: "thread/skills/updated",
    params: { threadId: "other-thread", names: ["other"] },
  });
  const root = s.roster.agents[0];
  assert.deepEqual(root.skills, ["terminal", "codex"]);
  assert.equal(root.cost, 0.00662);
  assert.equal(root.costPartial, false);
  assert.equal(root.tps, 42.5);
  assert.equal(root.work, 1200);
  assert.equal(root.roundtrips, 2);
  s.close();
});

test("Stream tracks child agent metrics/activity, supports earlier() paging, and normalizes image resource identity", async () => {
  const sha = "a".repeat(64);
  const machine = fakeMachine({
    "thread/resume": (params) => {
      assert.deepEqual(params, {
        threadId: "t-root",
        excludeTurns: true,
        initialTurnsPage: { sortDirection: "desc", limit: 5, itemsView: "full" },
      });
      return {
        thread: {
          id: "t-root",
          preview: "hi",
          cwd: "/repo",
          createdAt: 1700000000,
          status: { type: "idle" },
          turns: [],
        },
        model: "gpt-6.2-sol",
        turnsBackwardsCursor: "cursor-overridden-by-initial-page",
        initialTurnsPage: {
          data: [
            {
              id: "turn-2",
              status: "completed",
              itemsView: "full",
              items: [{ id: "a2", type: "agentMessage", text: "Newer turn" }],
              error: null,
            },
          ],
          nextCursor: "cursor-1",
          backwardsCursor: "reverse-cursor",
        },
      };
    },
    "thread/subtree/list": () => ({
      data: [
        { id: "t-root", cwd: "/repo", status: { type: "idle" }, turns: [] },
        {
          id: "t-worker",
          parentThreadId: "t-root",
          agentNickname: "worker",
          agentRole: "explorer",
          cwd: "/repo",
          status: { type: "active", activeFlags: [] },
          turns: [],
        },
      ],
    }),
    "thread/turns/list": ({ cursor, limit }) => {
      assert.equal(cursor, "cursor-1");
      assert.equal(limit, 20);
      return {
        data: [
          {
            id: "turn-1",
            status: "completed",
            itemsView: "full",
            items: [{ id: "a1", type: "agentMessage", text: "Older turn" }],
            error: null,
          },
        ],
        nextCursor: null,
        backwardsCursor: "another-reverse-cursor",
      };
    },
    "thread/changes/read": () => ({
      threadId: "t-root",
      workspace: "/repo",
      totalAdditions: 0,
      totalDeletions: 0,
      changes: [],
      files: [],
    }),
    image: () => new Blob(["ok"]),
  });

  const s = new Stream();
  s.connect("t-root", machine);
  await new Promise((r) => setTimeout(r, 15));

  assert.equal(s.hasEarlier, true);
  assert.equal(await s.earlier(), true);
  assert.equal(s.hasEarlier, false);
  assert.equal(s.main.order.length, 2);
  assert.equal(s.main.entries[s.main.order[0]].text, "Older turn");
  assert.equal(s.main.entries[s.main.order[1]].text, "Newer turn");

  machine.emitNotification({
    method: "thread/tokenUsage/updated",
    params: {
      threadId: "t-worker",
      tokenUsage: {
        total: {
          totalTokens: 500,
          inputTokens: 400,
          cachedInputTokens: 0,
          cacheWriteInputTokens: 0,
          outputTokens: 100,
          reasoningOutputTokens: 0,
        },
        last: {
          totalTokens: 500,
          inputTokens: 400,
          cachedInputTokens: 0,
          cacheWriteInputTokens: 0,
          outputTokens: 100,
          reasoningOutputTokens: 0,
        },
        modelContextWindow: 128000,
        estimatedCostUsd: 0.012,
        costPartial: true,
        outputTokensPerSecond: 33.3,
        workDurationMs: 3000,
        roundtrips: 1,
      },
    },
  });
  machine.emitNotification({
    method: "item/started",
    params: {
      threadId: "t-worker",
      turnId: "child-turn-1",
      item: {
        id: "child-cmd",
        type: "commandExecution",
        command: "/bin/bash -lc 'rg foo'",
        display: { script: "rg foo", segments: [], effectiveCwd: null },
        cwd: "/repo",
        status: "inProgress",
        exitCode: null,
      },
    },
  });
  const worker = s.roster.agents.find((a) => a.thread === "t-worker");
  assert.equal(worker?.cost, 0.012);
  assert.equal(worker?.costPartial, true);
  assert.equal(worker?.tps, 33.3);
  assert.equal(worker?.activity?.values?.[0]?.code, "rg foo");

  // Raw paths/URLs are omitted from body.image so http_resources matches by itemId + index;
  // SHA-256 hex identities pass through verbatim.
  const ac = new AbortController();
  await s.image({ itemId: "img-1", seq: 1, agent: "/root" }, 0, "/repo/shot.png", ac.signal);
  await s.image({ itemId: "img-2", seq: 2, agent: "/root" }, 0, sha, ac.signal);
  const imageReqs = machine.requests.filter((r) => r.kind === "image");
  assert.equal(imageReqs[0].params.body.image, "");
  assert.equal(imageReqs[1].params.body.image, sha);
  s.close();
});

test("ThreadControl handles slash commands locally instead of forwarding to turn/start", async () => {
  const machine = fakeMachine();
  const control = new ThreadControl();
  control.connect("t-slash", machine);

  assert.equal(await control.sendInput("/compact", []), undefined);
  assert.match(await control.sendInput("/archive", []), /Confirm archive/);
  assert.ok(!machine.requests.some((r) => r.method === "thread/archive"));
  assert.equal(await control.archive(), undefined);

  assert.equal(
    machine.requests.some((r) => r.method === "turn/start"),
    false,
  );
  assert.ok(machine.requests.some((r) => r.method === "thread/compact/start"));
  assert.ok(machine.requests.some((r) => r.method === "thread/archive"));

  const messages = [
    "/model example",
    "/effort high",
    "/reasoning low",
    "/tier standard",
    "/fast",
    "/rename New Title",
    "/title New Title",
  ];
  for (const text of messages) assert.match(await control.sendInput(text), /^Unknown command /);
  assert.equal(
    machine.requests.some(
      (r) =>
        r.method === "turn/start" ||
        r.method === "thread/settings/update" ||
        r.method === "thread/name/set",
    ),
    false,
  );
  control.release();
});
