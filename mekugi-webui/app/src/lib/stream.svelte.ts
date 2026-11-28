import { SvelteMap } from "svelte/reactivity";
import { commandValues } from "./command";
import { roleColor } from "./agents";
import { selectionAppendix, textUploadFrames, type InitialInput } from "./attachments";
import { threadStatusToStatus, type Machine } from "./hub.svelte";
import { errorText, TransportError, type Connection } from "./transport";
import type {
  Agent,
  AgentIdentity,
  AgentState,
  BackgroundTerminal,
  BackgroundTerminalsPage,
  CommandExecutionChangesUpdatedNotification,
  ContentReference,
  ContentResourceResponse,
  Entry,
  FileUpdateChange,
  HookRunSummary,
  Input,
  Preview,
  ReviewFile,
  ReviewedFile,
  Roster,
  SavedReview,
  ServerNotification,
  Session,
  Status,
  Thread,
  ThreadChangeFileDiff,
  ThreadChangesReadResponse,
  ThreadItem,
  ThreadListResponse,
  ThreadReadResponse,
  ThreadResumeResponse,
  ThreadStatus,
  ThreadTokenUsage,
  TextSpan,
  Turn,
  TurnsPage,
  Value,
} from "./protocol";

function formatJsonCode(value: unknown): string | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value === "string") return value.trim() ? value : undefined;
  try {
    const serialized = JSON.stringify(value, null, 2);
    return serialized && serialized !== "{}" && serialized !== "[]" ? serialized : undefined;
  } catch {
    return undefined;
  }
}

function linesOf(text: string | undefined): string[] | undefined {
  if (!text) return undefined;
  const trimmed = text.replace(/\n$/, "");
  return trimmed ? trimmed.split("\n") : undefined;
}

const recentTurns = { sortDirection: "desc", limit: 5, itemsView: "full" } as const;
const earlierTurns = { ...recentTurns, limit: 20 } as const;
const transcriptPageSize = 40;

function extractMcpTail(
  result: { content?: unknown[]; structuredContent?: unknown } | null | undefined,
  error: { message: string } | null | undefined,
): string[] | undefined {
  if (error?.message) return linesOf(error.message);
  if (!result) return undefined;
  const parts: string[] = [];
  for (const entry of result.content ?? []) {
    if (entry && typeof entry === "object" && "text" in entry && typeof entry.text === "string") {
      parts.push(entry.text);
    }
  }
  if (parts.length) return linesOf(parts.join("\n"));
  if (result.structuredContent !== undefined && result.structuredContent !== null) {
    return linesOf(formatJsonCode(result.structuredContent));
  }
  return undefined;
}

export function mcpToolEnvelope(item: Extract<ThreadItem, { type: "mcpToolCall" }>): string {
  return JSON.stringify(
    {
      arguments: item.arguments,
      ...(item.result == null ? {} : { result: item.result }),
      ...(item.error == null ? {} : { error: item.error }),
    },
    null,
    2,
  );
}

const inlineAudio = /^data:audio\/(?:aac|flac|mp4|mpeg|ogg|wav|wave|webm|x-wav);base64,/i;

function countDiffLines(
  diff: string | undefined,
  kind: string,
): { added: number; removed: number } {
  if (!diff) return { added: 0, removed: 0 };
  let added = 0;
  let removed = 0;
  const lines = diff.replace(/\n$/, "").split("\n");
  const nonEmpty = lines.length > 0 && !(lines.length === 1 && lines[0] === "");
  const hasUnifiedHeader = lines.some(
    (line) => line.startsWith("+++") || line.startsWith("---") || line.startsWith("@@"),
  );
  if ((kind === "add" || kind === "delete") && !hasUnifiedHeader) {
    if (!nonEmpty) return { added: 0, removed: 0 };
    return kind === "add"
      ? { added: lines.length, removed: 0 }
      : { added: 0, removed: lines.length };
  }
  for (const line of lines) {
    if (line.startsWith("+++") || line.startsWith("---") || line.startsWith("@@")) {
      continue;
    }
    if (line.startsWith("+")) {
      added++;
    } else if (line.startsWith("-")) {
      removed++;
    }
  }
  return { added, removed };
}

export type { Connection } from "./transport";

export interface Feed {
  attachment?: string;
  order: number[];
  entries: Record<number, Entry>;
  revision?: number;
}

export interface Gone {
  status: number;
  text: string;
}

const empty: Feed = { order: [], entries: {} };
const encoder = new TextEncoder();
const decoder = new TextDecoder();

function threadStatusToAgentState(status: ThreadStatus): AgentState {
  switch (status.type) {
    case "active":
      return status.activeFlags.includes("waitingOnApproval") ||
        status.activeFlags.includes("waitingOnUserInput")
        ? "waiting"
        : "working";
    case "systemError":
      return "error";
    case "idle":
      return "idle";
    case "notLoaded":
      return "done";
  }
}

export function itemToEntry(
  item: ThreadItem,
  seq: number,
  turnIndex: number,
  turnId: string,
  agent = "/root",
  live = false,
  threadId?: string,
): Entry | undefined {
  const base: Entry = {
    seq,
    position: seq,
    itemId: item.id,
    turnId,
    agent,
    observed: item.startedAtMs ?? undefined,
    content: { turnId, itemId: item.id },
  };
  switch (item.type) {
    case "userMessage": {
      const parts: string[] = [];
      const images: string[] = [];
      const spans: TextSpan[] = [];
      let offset = 0;
      for (const input of item.content) {
        if (input.type === "text") {
          const bytes = encoder.encode(input.text);
          let lastSelection = -1;
          for (const element of input.text_elements ?? []) {
            const { start, end } = element.byteRange;
            const label = element.placeholder;
            if (!label || decoder.decode(bytes.subarray(start, end)) !== label) continue;
            const reference = label.startsWith("[Selected ");
            const kind = reference ? 2 : label.startsWith("[Image ") ? 0 : 1;
            spans.push({ Start: offset + start, End: offset + end, Kind: kind });
            if (reference) lastSelection = Math.max(lastSelection, end);
          }
          if (input.text.startsWith(selectionAppendix) && spans.some((span) => span.Kind === 2))
            continue;
          if (textUploadFrames(input.text).length) continue;
          const tail = lastSelection < 0 ? "" : decoder.decode(bytes.subarray(lastSelection));
          const appendix = tail.indexOf(selectionAppendix);
          const display =
            appendix < 0
              ? input.text
              : decoder.decode(bytes.subarray(0, lastSelection)) +
                tail.slice(0, appendix).trimEnd();
          parts.push(display);
          offset += encoder.encode(display).length + 1;
        } else if (input.type === "localImage") images.push(input.path);
        else if (input.type === "image" && "url" in input) images.push(input.url);
        else if (input.type === "skill" || input.type === "mention") {
          const display = `${input.type === "skill" ? "$" : "@"}${input.name}`;
          parts.push(display);
          offset += encoder.encode(display).length + 1;
        }
      }
      for (let i = spans.filter((span) => span.Kind === 0).length; i < images.length; i++) {
        const label = `[Image ${i + 1}]`;
        parts.push(label);
        spans.push({ Start: offset, End: offset + label.length, Kind: 0 });
        offset += label.length + 1;
      }
      return {
        ...base,
        kind: "user",
        turn: turnIndex,
        text: parts.join("\n"),
        ...(spans.length ? { spans } : {}),
        images: images.length ? images : undefined,
      };
    }
    case "agentMessage":
      return {
        ...base,
        kind: "assistant",
        text: item.text,
      };
    case "functionCallOutput": {
      // Only the canonical exec diagnostic is a user-facing transcript item.
      if (
        item.namespace !== "functions" ||
        item.name !== "exec" ||
        !item.id.startsWith("code-mode-error:")
      )
        return;
      const diagnostic = typeof item.output === "string" ? item.output.trimEnd() : "";
      return {
        ...base,
        kind: "error",
        outcome: "error",
        values: [
          {
            kind: "error",
            verb: "Error",
            label: "exec script failed",
            failed: true,
            tail: diagnostic ? diagnostic.split("\n") : undefined,
          },
        ],
      };
    }
    case "plan":
      return {
        ...base,
        kind: "plan",
        text: item.text,
      };
    case "reasoning": {
      const sections = (item.summary ?? []).map((part) => part.trim()).filter(Boolean);
      const values: Value[] = sections.map((body, index) => {
        const heading = body.match(/^\*\*([^\n*]+)\*\*/)?.[1]?.trim();
        return {
          kind: "reasoning",
          label: heading || undefined,
          body,
          live: live && index === sections.length - 1,
        };
      });
      return {
        ...base,
        kind: "reasoning",
        running: live || undefined,
        values,
      };
    }
    case "commandExecution": {
      const running = item.status === "inProgress";
      const failed = item.status === "failed" || item.status === "declined";
      return {
        ...base,
        kind: "tool",
        running,
        outcome: failed ? "error" : undefined,
        values: commandValues(item),
      };
    }
    case "fileChange": {
      const running = item.status === "inProgress";
      const failed = item.status === "failed" || item.status === "declined";
      const values: Value[] = item.changes.length
        ? item.changes.map((c) => {
            const verb =
              c.kind.type === "add"
                ? "Create"
                : c.kind.type === "delete"
                  ? "Delete"
                  : c.kind.type === "update" && c.kind.move_path
                    ? "Move"
                    : "Edit";
            const path =
              c.kind.type === "update" && c.kind.move_path
                ? `${c.path} => ${c.kind.move_path}`
                : c.path;
            const { added, removed } = countDiffLines(c.diff, c.kind.type);
            const label = added || removed ? `+${added} −${removed}` : undefined;
            return {
              kind: "op",
              verb,
              path,
              label,
              code: c.diff || undefined,
              lang: c.diff ? "diff" : undefined,
              foldSource: c.diff ? true : undefined,
              running,
              failed,
            };
          })
        : [{ kind: "op", verb: "Edit", running, failed }];
      return {
        ...base,
        kind: "tool",
        running,
        outcome: failed ? "error" : undefined,
        values,
      };
    }
    case "mcpToolCall": {
      const running = item.status === "inProgress";
      const failed = item.status === "failed";
      const tail = extractMcpTail(item.result, item.error);
      const value: Value = {
        kind: "op",
        verb: "Call",
        label: `${item.server}:${item.tool}`,
        code: mcpToolEnvelope(item),
        lang: "json",
        opaqueJSON: true,
        running,
        failed,
        duration: item.durationMs ?? undefined,
        tail,
      };
      return {
        ...base,
        kind: "tool",
        running,
        outcome: failed ? "error" : undefined,
        values: [value],
      };
    }
    case "dynamicToolCall": {
      const running = item.status === "inProgress";
      const failed = item.status === "failed" || item.success === false;
      const argsCode = formatJsonCode(item.arguments);
      const parts: string[] = [];
      const images: string[] = [];
      const audio: string[] = [];
      for (const content of item.contentItems ?? []) {
        if (content.type === "inputText") parts.push(content.text);
        else if (content.type === "inputImage") images.push(content.imageUrl);
        else if (inlineAudio.test(content.audioUrl)) audio.push(content.audioUrl);
        else parts.push("[Audio unavailable]");
      }
      const label = item.namespace ? `${item.namespace}:${item.tool}` : item.tool;
      const value: Value = {
        kind: "op",
        verb: "Call",
        label,
        code: argsCode,
        lang: argsCode ? "json" : undefined,
        running,
        failed,
        duration: item.durationMs ?? undefined,
        tail: linesOf(parts.join("\n")),
      };
      return {
        ...base,
        kind: "tool",
        running,
        outcome: failed ? "error" : undefined,
        values: [value],
        images: images.length ? images : undefined,
        audio: audio.length ? audio : undefined,
      };
    }
    case "webSearch":
      return {
        ...base,
        kind: "tool",
        values: [{ kind: "search", verb: "search", label: item.query }],
      };
    case "imageView":
      return {
        ...base,
        kind: "tool",
        images: [item.path],
        values: [{ kind: "read", verb: "View", path: item.path }],
      };
    case "imageGeneration": {
      const source = item.savedPath || ("result" in item && item.result ? item.id : undefined);
      const running = item.status === "in_progress" || item.status === "generating";
      const failed = item.status === "failed" || !!item.failure;
      return {
        ...base,
        kind: "tool",
        running,
        outcome: failed ? "error" : undefined,
        images: source ? [source] : undefined,
        values: [
          {
            kind: "read",
            verb: source ? "Generated" : "Generate",
            label: "image",
            path: item.savedPath,
            running,
            failed,
          },
        ],
      };
    }
    case "subAgentActivity": {
      const target = item.agentPath;
      const owner = threadId ?? agent;
      const traffic = {
        started: { kind: "start", from: owner, to: target, detail: "Started" },
        interacted: { kind: "message", from: owner, to: target, detail: "Messaged" },
        interrupted: { kind: "interrupted", from: owner, to: target, detail: "Interrupted" },
        completed: { kind: "final", from: target, to: owner, detail: "Finished" },
      }[item.kind];
      const assignment = item.assignment ?? undefined;
      return {
        ...base,
        kind: traffic.kind === "start" ? "start" : "agent",
        assignment: !!assignment,
        assignmentTo: assignment ? target : undefined,
        assignmentText: assignment,
        identities: [
          {
            name: target,
            label: target.split("/").at(-1) || target,
            aliases: [target, item.agentThreadId],
          },
        ],
        traffic,
      };
    }
    case "agentMail": {
      const reply = item.kind === "reply";
      return {
        ...base,
        kind: reply ? "reply" : "agent",
        traffic: {
          kind: reply ? "final" : "message",
          from: item.sender,
          to: item.recipient,
          detail: reply ? "Reply delivered" : "Message received",
          incoming: true,
        },
        values: [
          {
            kind: "message",
            body: item.text,
            summary: item.summary ?? undefined,
            details: item.details ?? undefined,
          },
        ],
      };
    }
    case "collabAgentToolCall": {
      const from = item.senderThreadId;
      const [to, ...otherRecipients] = item.receiverThreadIds;
      const status = item.status;
      if (item.tool === "wait") {
        return {
          ...base,
          kind: "agent",
          values: [
            {
              kind: "progress",
              from,
              label: status === "inProgress" ? "Waiting for" : "Waited for",
              live: status === "inProgress",
              waitReason: status === "interrupted" || status === "failed" ? status : undefined,
              waits: item.receiverThreadIds.map((name) => ({
                name,
                status: item.agentsStates[name]?.status,
              })),
            },
            ...Object.entries(item.agentsStates).flatMap(([name, state]) =>
              state?.message
                ? [{ kind: "message", from: name, to: from, body: state.message }]
                : [],
            ),
          ],
        };
      }
      const failed = status === "failed";
      const kind = failed
        ? "error"
        : status === "interrupted" || item.tool === "interruptAgent"
          ? "interrupted"
          : item.tool === "spawnAgent" || item.tool === "resumeAgent"
            ? "start"
            : "message";
      const detail = {
        spawnAgent: "Start",
        resumeAgent: "Resume",
        sendInput: "Send input",
        sendMessage: "Message",
        followupTask: "Follow up",
        closeAgent: "Close",
        interruptAgent: "Interrupt",
        listAgents: "List agents",
      }[item.tool];
      const body = item.prompt ?? undefined;
      return {
        ...base,
        kind: kind === "start" ? "start" : "agent",
        outcome: failed ? "error" : undefined,
        text: body,
        traffic: { kind, from, to, otherRecipients, detail },
      };
    }
    case "contextCompaction": {
      const reset = item.summaryFree || item.sourceAcquisition?.purpose === "reload";
      const label = live
        ? reset
          ? "Resetting context"
          : "Compacting context"
        : reset
          ? "Context reset"
          : "Context compacted";
      return {
        ...base,
        kind: "compaction",
        text: label,
        values: [
          {
            kind: "compaction",
            label,
            body: item.summary ?? undefined,
            detailsAvailable: live ? undefined : {},
          },
        ],
      };
    }
    case "enteredReviewMode":
    case "exitedReviewMode":
      return {
        ...base,
        kind: "system",
        text:
          item.review ||
          (item.type === "enteredReviewMode" ? "Entered review mode" : "Exited review mode"),
      };
    case "hookPrompt":
      return {
        ...base,
        kind: "system",
        text: item.fragments.map((f) => f.text).join("\n"),
      };
    default:
      return {
        ...base,
        kind: "system",
        text: item.type,
      };
  }
}

export interface HookRecord {
  key: string;
  id: string;
  turnId: string | null;
  afterItemCount: number;
  run: HookRunSummary;
}

export function hookRunToEntry(
  run: HookRunSummary,
  seq: number,
  turnId?: string,
  agent = "/root",
): Entry {
  const running = run.status === "running";
  const failed = run.status === "failed" || run.status === "blocked" || run.status === "stopped";
  const sourceName = run.sourcePath ? run.sourcePath.split("/").at(-1) : undefined;
  const labelParts = [
    run.eventName,
    sourceName ? `\`${sourceName}\`` : undefined,
    run.statusMessage || undefined,
  ].filter(Boolean);
  const tail = run.entries
    .flatMap((entry) => entry.text.split("\n"))
    .filter((line) => line.length > 0);
  if (failed && !tail.length && run.statusMessage) tail.push(run.statusMessage);
  const rawStarted = Number(run.startedAt);
  const observed =
    Number.isFinite(rawStarted) && rawStarted > 0
      ? rawStarted < 1e12
        ? rawStarted * 1000
        : rawStarted
      : undefined;
  return {
    seq,
    position: seq,
    itemId: `hook:${run.id}`,
    turnId,
    agent,
    observed,
    kind: "hook",
    running: running || undefined,
    outcome: failed ? "error" : undefined,
    values: [
      {
        kind: "hook",
        label: labelParts.join(" · "),
        duration: run.durationMs != null ? Number(run.durationMs) : undefined,
        running,
        failed,
        tail: tail.length ? tail : undefined,
        hook: {
          event: run.eventName,
          name: sourceName,
          message: run.statusMessage ?? undefined,
          status: run.status,
          hasError: failed,
          blocked: run.status === "blocked",
        },
      },
    ],
  };
}

export function buildFeed(
  turns: Turn[],
  attachment?: string,
  liveItemIds?: ReadonlySet<string>,
  hooks: readonly HookRecord[] = [],
  indexItem?: (turn: Turn, itemIndex: number, entry: Entry) => void,
): Feed {
  const entries: Record<number, Entry> = {};
  const order: number[] = [];
  let seq = 0;
  const emittedHooks = new Set<string>();
  const emitHook = (record: HookRecord) => {
    if (emittedHooks.has(record.key)) return;
    emittedHooks.add(record.key);
    seq++;
    entries[seq] = {
      ...hookRunToEntry(record.run, seq, record.turnId ?? undefined),
      rowKey: `hook:${record.key}`,
    };
    order.push(seq);
  };
  const hooksByTurn = new Map<string, HookRecord[]>();
  for (const hook of hooks) {
    if (!hook.turnId) continue;
    const list = hooksByTurn.get(hook.turnId);
    if (list) list.push(hook);
    else hooksByTurn.set(hook.turnId, [hook]);
  }
  for (const list of hooksByTurn.values()) {
    if (list.length > 1) list.sort((a, b) => a.afterItemCount - b.afterItemCount);
  }
  for (let t = 0; t < turns.length; t++) {
    const turn = turns[t];
    const turnHooks = hooksByTurn.get(turn.id) ?? [];
    let hookIdx = 0;
    for (let i = 0; i < turn.items.length; i++) {
      while (hookIdx < turnHooks.length && turnHooks[hookIdx].afterItemCount <= i) {
        emitHook(turnHooks[hookIdx++]);
      }
      const item = turn.items[i];
      const entry = itemToEntry(
        item,
        seq + 1,
        t + 1,
        turn.id,
        "/root",
        !!liveItemIds?.has(item.id),
        attachment,
      );
      if (!entry) continue;
      entries[++seq] = entry;
      indexItem?.(turn, i, entry);
      order.push(seq);
    }
    while (hookIdx < turnHooks.length) emitHook(turnHooks[hookIdx++]);
  }
  for (const hook of hooks) emitHook(hook);
  return { attachment, order, entries, revision: seq };
}

// apply_patch sends raw contents for additions and deletions, unlike shell observations.
function nativePatchDiff(kind: "add" | "delete" | "update", diff: string): string {
  if (kind === "update" || !diff) return diff;
  const header = kind === "add" ? "@@ -0 +1 @@" : "@@ -1 +0 @@";
  const sign = kind === "add" ? "+" : "-";
  const lines = diff
    .replace(/\n$/, "")
    .split("\n")
    .map((line) => sign + line);
  return [header, ...lines].join("\n");
}

function previewFile({ path, kind, diff }: FileUpdateChange): ReviewFile {
  const movedTo = kind.type === "update" ? kind.move_path : undefined;
  return {
    Path: path,
    BeforePath: kind.type === "add" ? undefined : path,
    AfterPath: kind.type === "delete" ? undefined : (movedTo ?? path),
    Action: kind.type === "add" ? "A" : kind.type === "delete" ? "D" : movedTo ? "R" : "M",
    Diff: diff,
  };
}

function previewId(thread: string, turn: string, item: string): string {
  return JSON.stringify([thread, turn, item]);
}

const changesPageSize = 20;
const receiptPageSize = 100;

function capturedEditValue(file: ThreadChangeFileDiff): Value {
  return {
    kind: "op",
    verb:
      file.status === "A"
        ? "Create"
        : file.status === "D"
          ? "Delete"
          : file.status === "R"
            ? "Move"
            : "Edit",
    path: file.movedFrom ? `${file.movedFrom} => ${file.path}` : file.path,
    label: file.additions || file.deletions ? `+${file.additions} −${file.deletions}` : undefined,
    effect: true,
  };
}

function changeFileReview(file: ThreadChangeFileDiff): ReviewFile {
  const path = file.absolutePath ?? file.path;
  return {
    Path: path,
    BeforePath: file.status === "A" ? undefined : (file.movedFrom ?? path),
    AfterPath: file.status === "D" ? undefined : path,
    Action: file.status,
    Diff: file.unifiedDiff,
    Binary: file.isBinary,
    Incomplete: file.coverageNotes?.join("; "),
    Added: file.additions,
    Removed: file.deletions,
    Scope: file.scope,
  };
}

function changesToReview(
  response: ThreadChangesReadResponse,
  revision: number,
  caller: string,
  callerName: (id: string) => string,
): SavedReview {
  const origins = new Map(
    response.changes.map((change) => [
      change.changeId,
      {
        Change: change.changeId,
        Caller: change.authorThreadId ? callerName(change.authorThreadId) : "",
        Source: change.sourceLabel,
      },
    ]),
  );
  const files: ReviewedFile[] = response.files.map((file) => {
    const fileOrigins = (file.changeIds ?? [])
      .map((id) => origins.get(id))
      .filter((origin) => origin !== undefined);
    const path = file.absolutePath ?? file.path;
    const key = caller ? JSON.stringify([path, file.changeIds]) : path;
    return {
      key,
      path,
      status: file.status,
      chunks: [
        {
          Key: key,
          Status: file.status,
          Stream: "saved",
          Workspace: response.workspace ?? "",
          CaptureOrder: 0,
          Change: "",
          Caller: "",
          Source: "",
          Highlighted: false,
          Review: changeFileReview(file),
        },
      ],
      origins: fileOrigins,
      counts: { Added: file.additions, Removed: file.deletions },
    };
  });
  const net = { Added: response.totalAdditions, Removed: response.totalDeletions };
  const callers = (response.callers ?? []).map((entry) => ({
    caller: entry.threadId,
    count: entry.fileCount,
    counts: { Added: entry.additions, Removed: entry.deletions },
  }));
  return {
    caller,
    fileCount: response.fileCount ?? files.length,
    revision,
    workspace: response.workspace ?? "",
    coverage: response.incomplete ? "Partial capture" : "",
    callers,
    counts: {
      Callers: Object.fromEntries(callers.map((entry) => [entry.caller, entry.counts])),
      Net: files.length ? net : null,
    },
    files,
  };
}

function usageFields(usage: ThreadTokenUsage) {
  const measured = !usage.usageUnavailable;
  return {
    inputTokens: measured ? usage.total.inputTokens : undefined,
    cachedInputTokens: measured ? usage.total.cachedInputTokens : undefined,
    outputTokens: measured ? usage.total.outputTokens : undefined,
    reasoningOutputTokens: measured ? usage.total.reasoningOutputTokens : undefined,
    usagePartial: usage.usagePartial,
    cost: usage.estimatedCostUsd,
    costPartial: usage.costPartial,
    tps: usage.outputTokensPerSecond,
    work: usage.workDurationMs,
    workSince: usage.workStartedAt == null ? undefined : usage.workStartedAt * 1000,
    lastResponse: usage.lastResponseAt == null ? undefined : usage.lastResponseAt * 1000,
    lastCompleted: usage.lastCompletedAt == null ? undefined : usage.lastCompletedAt * 1000,
    turns: usage.turns ?? undefined,
    roundtrips: usage.roundtrips ?? undefined,
    roundtripsPartial: usage.roundtripsPartial,
    context: usage.last.totalTokens || (usage.modelContextWindow ? 0 : undefined),
    window: usage.modelContextWindow ?? undefined,
  };
}

function threadAgentPath(thread: Thread): string | undefined {
  const source = thread.source;
  if (typeof source !== "object" || !source || !("subAgent" in source)) return undefined;
  const sub = source.subAgent;
  if (typeof sub !== "object" || !sub || !("thread_spawn" in sub)) return undefined;
  return sub.thread_spawn.agent_path ?? undefined;
}

function threadToAgent(thread: Thread, depth: number): Agent {
  const canonicalPath = depth === 0 ? "/root" : threadAgentPath(thread);
  const name = canonicalPath || thread.agentNickname || thread.id;
  const label =
    depth === 0
      ? "main"
      : canonicalPath
        ? canonicalPath.replace(/^\/root\//, "")
        : thread.agentNickname || thread.name || thread.id;
  const usage = thread.tokenUsage ? usageFields(thread.tokenUsage) : undefined;
  return {
    name,
    label,
    aliases: [
      name,
      thread.id,
      ...(thread.agentNickname && thread.agentNickname !== name ? [thread.agentNickname] : []),
    ],
    thread: thread.id,
    role: thread.agentRole ?? undefined,
    depth,
    state: threadStatusToAgentState(thread.status),
    skills: thread.extra?.skills?.length ? thread.extra.skills : undefined,
    ...usage,
    context: thread.contextTokens ?? usage?.context,
    window: thread.contextWindow ?? usage?.window,
  };
}

const maxCachedHookThreads = 20;
const maxRoutineHooksPerThread = 200;
const maxFailedHooksPerThread = 200;
type HookCache = Map<string, { records: HookRecord[]; sequence: number }>;
type ProcessChange = Extract<ServerNotification, { method: "thread/backgroundTerminals/changed" }>;

function threadMayHaveProcesses(thread: Thread): boolean {
  if (thread.status.type === "notLoaded") return false;
  const count = thread.extra?.backgroundTerminalCount;
  return typeof count === "number" ? count > 0 : true;
}
const processChangeReads = new WeakMap<ProcessChange, Promise<BackgroundTerminalsPage>>();

function isHookFailed(run: HookRunSummary): boolean {
  return run.status === "failed" || run.status === "blocked" || run.status === "stopped";
}

function trimHooks(hooks: HookRecord[]): HookRecord[] {
  let routine = 0;
  let failed = 0;
  for (const h of hooks) {
    if (isHookFailed(h.run)) failed++;
    else if (h.run.status !== "running") routine++;
  }
  if (routine <= maxRoutineHooksPerThread && failed <= maxFailedHooksPerThread) return hooks;
  let dropRoutine = Math.max(0, routine - maxRoutineHooksPerThread);
  let dropFailed = Math.max(0, failed - maxFailedHooksPerThread);
  return hooks.filter((h) => {
    if (h.run.status === "running") return true;
    if (isHookFailed(h.run)) {
      if (dropFailed > 0) {
        dropFailed--;
        return false;
      }
      return true;
    }
    if (dropRoutine > 0) {
      dropRoutine--;
      return false;
    }
    return true;
  });
}

export class Stream {
  session = $state.raw<Session | undefined>();
  input = $state.raw<Input | undefined>();
  status = $state.raw<Status>({});
  // Structural snapshot. Use entry(seq) for live content.
  main = $state.raw<Feed>(empty);
  roster = $state.raw<Roster>({ total: 0, agents: [] });
  family = $state.raw<{ root: string; threads: string[] } | undefined>();
  review = $state.raw<SavedReview | null>(null);
  reviewCaller = $state("");
  reviewIncludeExternal = $state(false);
  reviewCursor = $state<string>();
  reviewError = $state<string>();
  reviewLoading = $state(false);
  reviewOverview = $state.raw<{
    fileCount: number;
    callers: SavedReview["callers"];
    counts: SavedReview["counts"];
  }>({
    fileCount: 0,
    callers: [],
    counts: { Callers: {}, Net: null },
  });
  live = $state.raw<Preview[]>([]);
  processes = $state.raw<BackgroundTerminal[]>([]);
  activeTurnId = $state<string | undefined>();

  get latestCompletedTurnId(): string | undefined {
    const turn = this.#turns.at(-1);
    return turn?.status === "completed" ? turn.id : undefined;
  }

  // Stable row seeds for grouping; each mounted row reads its current entry separately.
  transcriptEntries = $derived.by(() => {
    const { order, entries } = this.main;
    return order.slice(this.#hiddenEntries).map((seq) => entries[seq]);
  });
  hasEarlier = $derived.by(() => this.#hiddenEntries > 0 || !!this.#turnsBackwardsCursor);
  loadingEarlier = $state(false);
  hydrating = $state(false);
  scroll?: { top: number; pinned: boolean; anchor?: { id: string; offset: number } };
  byName = $derived(new Map(this.roster.agents.map((agent) => [agent.name, agent])));
  identities = $derived.by(() => {
    const identities = new Map<string, AgentIdentity>();
    const remember = (identity: AgentIdentity) => {
      for (const alias of identity.aliases) identities.set(alias, identity);
    };
    for (const agent of this.roster.agents) {
      for (const identity of agent.identities ?? []) remember(identity);
      remember({ name: agent.name, label: agent.label, aliases: agent.aliases ?? [agent.name] });
    }
    for (const seq of this.main.order) {
      for (const identity of this.main.entries[seq]?.identities ?? []) {
        const known = identity.aliases.map((alias) => identities.get(alias)).find(Boolean);
        for (const alias of identity.aliases) identities.set(alias, known ?? identity);
      }
    }
    return identities;
  });
  roles = $state.raw<string[]>([]);
  error = $state<string | undefined>();
  notice = $state<string | undefined>();
  gone = $state.raw<Gone | undefined>();
  jumps = $state(0);

  #id?: string;
  #machine = $state.raw<Machine>();
  #turns: Turn[] = [];
  #entries = $state.raw(new SvelteMap<number, Entry>());
  #deltaTargets = new Map<
    string,
    Map<string, { items: ThreadItem[]; index: number; seq: number }>
  >();
  #firstOwnTurn?: string;
  #turnsBackwardsCursor = $state<string>();
  #hiddenEntries = $state(0);
  #liveItemIds = new Set<string>();
  #hooks: HookRecord[] = [];
  #hooksByThread: HookCache;
  #hookSeq = 0;
  #reviewRevision = 0;
  #editReceipts = new SvelteMap<string, Value[]>();
  #workspaces = new Map<string, string>();
  #processReads = new Map<string, symbol>();
  #stopNotifications?: () => void;
  #stopOpen?: () => void;
  #requests = new AbortController();
  #selected = false;
  #subscribed = false;
  #maySubscribe = false;
  #hydration?: Promise<void>;
  #resumeRequest?: Promise<ThreadResumeResponse>;
  #predecessor?: Promise<void>;
  #catchupNotifications?: ServerNotification[];

  constructor(hooksByThread: HookCache = new Map()) {
    this.#hooksByThread = hooksByThread;
  }

  get connection(): Connection {
    return this.#machine?.connection ?? "connecting";
  }

  get machine(): Machine | undefined {
    return this.#machine;
  }

  get threadId(): string | undefined {
    return this.#id;
  }

  entry(seq: number): Entry | undefined {
    const entry = this.#entries.get(seq);
    if (!entry?.turnId || !entry.itemId || entry.running || !this.#id) return entry;
    const effects = this.#editReceipts.get(previewId(this.#id, entry.turnId, entry.itemId));
    return effects?.length ? { ...entry, values: [...(entry.values ?? []), ...effects] } : entry;
  }

  commandExecution(turnId: string, itemId: string) {
    const entries = this.#entries;
    const target = this.#deltaTargets.get(turnId)?.get(itemId);
    // Subscribe to the row's updates without storing the full output in the row.
    if (!target || !entries.get(target.seq)) return;
    const item = target.items[target.index];
    return item.type === "commandExecution" ? item : undefined;
  }

  connect(
    id: string,
    machine: Machine,
    options: {
      started?: InitialInput["started"];
      firstOwnTurn?: string;
      familySource?: Stream;
      predecessor?: Promise<void>;
    } = {},
  ): void {
    const { started, firstOwnTurn, familySource, predecessor } = options;
    this.#id = id;
    this.#machine = machine;
    this.#selected = true;
    this.#firstOwnTurn = firstOwnTurn;
    this.#predecessor = predecessor;
    if (familySource?.machine === machine && familySource.family?.threads.includes(id)) {
      this.roster = familySource.roster;
      this.family = familySource.family;
      this.processes = familySource.processes;
      this.roles = familySource.roles;
      this.#workspaces = new Map(familySource.#workspaces);
    }
    const cacheKey = `${machine.id}:${id}`;
    const cachedHooks = this.#hooksByThread.get(cacheKey);
    // Running hooks may have finished across a subscription gap.
    if (cachedHooks) {
      this.#hooks = cachedHooks.records.filter((hook) => hook.run.status !== "running");
      this.#hookSeq = cachedHooks.sequence;
      this.#rememberHooks(cacheKey, this.#hooks);
    }
    this.#stopNotifications = machine.onNotification((n) => this.#onNotification(n));
    this.#stopOpen = machine.onOpen(() => {
      this.#requests.abort();
      this.#requests = new AbortController();
      this.#hydration = this.#resumeRequest = undefined;
      this.#catchupNotifications = undefined;
      this.#subscribed = this.#maySubscribe = false;
      this.loadingEarlier = this.reviewLoading = this.hydrating = false;
      this.error = undefined;
      if (this.#selected) void this.refresh();
    });
    if (started && machine.connection === "open") {
      // thread/start already subscribed this connector. Resume requires a saved
      // turn, so start observing live events before submitting the first input.
      this.#subscribed = this.#maySubscribe = true;
      this.#applyThread(started.thread, {
        model: started.model,
        effort: started.reasoningEffort ?? undefined,
        serviceTier: started.serviceTier ?? undefined,
        permissionProfile: started.activePermissionProfile?.id ?? null,
      });
    } else if (machine.connection === "open") {
      void this.refresh();
    }
  }

  setSelected(selected: boolean): void {
    this.#selected = selected;
    if (selected && !this.#subscribed && this.#machine?.connection === "open") {
      void this.refresh();
    }
  }

  async refresh(keepEarlier = true): Promise<void> {
    if (!this.#id || this.#machine?.connection !== "open") return;
    if (this.#hydration) return this.#hydration;
    this.hydrating = true;
    const signal = this.#requests.signal;
    const hydration = this.#hydrate(this.#id, this.#machine, signal, keepEarlier);
    this.#hydration = hydration;
    try {
      await hydration;
    } finally {
      if (this.#hydration === hydration) {
        this.#hydration = undefined;
        this.hydrating = false;
      }
    }
  }

  async earlier(): Promise<boolean> {
    const id = this.#id;
    const machine = this.#machine;
    const cursor = this.#turnsBackwardsCursor;
    if (!id || this.loadingEarlier || this.hydrating) return false;
    if (this.#hiddenEntries) {
      const previous = this.#hiddenEntries;
      this.#hiddenEntries = Math.max(0, this.#hiddenEntries - transcriptPageSize);
      this.#loadRowReceipts(this.#hiddenEntries, previous);
      return true;
    }
    if (machine?.connection !== "open" || !cursor) return false;
    const signal = this.#requests.signal;
    this.loadingEarlier = true;
    try {
      const page = await machine.request<TurnsPage>(
        "thread/turns/list",
        { threadId: id, cursor, ...earlierTurns },
        { signal },
      );
      if (signal.aborted || this.#turnsBackwardsCursor !== cursor) return false;
      const nextCursor = page.nextCursor;
      this.#turnsBackwardsCursor = nextCursor && nextCursor !== cursor ? nextCursor : undefined;
      const existingIds = new Set(this.#turns.map((t) => t.id));
      const older: Turn[] = [];
      for (let i = (page.data?.length ?? 0) - 1; i >= 0; i--) {
        const t = page.data[i];
        if (!existingIds.has(t.id)) older.push({ ...t, items: [...t.items] });
      }
      if (!older.length) return false;
      const retainedCount = this.main.order.length;
      this.#turns = [...older, ...this.#turns];
      this.#rebuildMain();
      this.#hiddenEntries = Math.max(
        0,
        this.main.order.length - retainedCount - transcriptPageSize,
      );
      this.#loadRowReceipts(this.#hiddenEntries, this.main.order.length - retainedCount);
      return true;
    } catch {
      return false;
    } finally {
      if (!signal.aborted) this.loadingEarlier = false;
    }
  }

  async #hydrate(
    id: string,
    machine: Machine,
    signal: AbortSignal,
    keepEarlier: boolean,
  ): Promise<void> {
    let catchupNotifications: ServerNotification[] | undefined;
    try {
      if (this.#predecessor) {
        await this.#predecessor;
        if (signal.aborted || machine.connection !== "open") return;
        this.#predecessor = undefined;
      }
      let thread: Thread;
      let model: string | undefined;
      let effort: string | undefined;
      let serviceTier: string | undefined;
      let permissionProfile: string | null | undefined;
      let turnsBackwardsCursor: string | undefined;
      let initialPage: TurnsPage | null | undefined;
      try {
        this.#maySubscribe = true;
        // Client cancellation cannot undo a sent resume. Let eviction wait for
        // the wire result before unsubscribing or letting this ID reopen.
        const request = machine.request<ThreadResumeResponse>("thread/resume", {
          threadId: id,
          excludeTurns: true,
          initialTurnsPage: recentTurns,
        });
        this.#resumeRequest = request;
        const resumed = await request.finally(() => {
          if (this.#resumeRequest === request) this.#resumeRequest = undefined;
        });
        if (signal.aborted) return;
        this.#subscribed = true;
        thread = resumed.thread;
        model = resumed.model || thread.model || undefined;
        effort = resumed.reasoningEffort ?? thread.reasoningEffort ?? undefined;
        serviceTier = resumed.serviceTier ?? thread.serviceTier ?? undefined;
        permissionProfile = resumed.activePermissionProfile?.id ?? null;
        turnsBackwardsCursor = resumed.turnsBackwardsCursor ?? undefined;
        initialPage = resumed.initialTurnsPage;
      } catch (err) {
        if (signal.aborted) return;
        if (err instanceof TransportError && (err.status === 404 || err.status === 410)) {
          this.gone = { status: err.status, text: err.message };
          return;
        }
        const read = await machine.request<ThreadReadResponse>(
          "thread/read",
          { threadId: id, includeTurns: false },
          { signal },
        );
        thread = read.thread;
        initialPage = await machine.request<TurnsPage>(
          "thread/turns/list",
          { threadId: id, ...recentTurns },
          { signal },
        );
        model = thread.model ?? undefined;
        effort = thread.reasoningEffort ?? undefined;
        serviceTier = thread.serviceTier ?? undefined;
      }
      if (signal.aborted) return;
      this.gone = undefined;
      this.error = undefined;
      let overlap = -1;
      const retained = keepEarlier ? this.#turns : [];
      if (initialPage) {
        const newer = [...initialPage.data];
        turnsBackwardsCursor = initialPage.nextCursor ?? undefined;
        const retainedPositions = new Map(retained.map((turn, index) => [turn.id, index]));
        const first = newer.at(-1);
        overlap = first ? (retainedPositions.get(first.id) ?? -1) : -1;
        if (first && overlap < 0 && retained.length && turnsBackwardsCursor) {
          // Don't publish a gap or overwrite changes after the resume snapshot.
          // Keep the old transcript visible while joining it to the new tail.
          this.#catchupNotifications = catchupNotifications = [];
          let cursor: string | undefined = turnsBackwardsCursor;
          while (cursor) {
            let page: TurnsPage;
            try {
              page = await machine.request<TurnsPage>(
                "thread/turns/list",
                { threadId: id, cursor, ...earlierTurns },
                { signal },
              );
            } catch (err) {
              if (signal.aborted || machine.connection !== "open") return;
              // The pages fetched so far are contiguous and still usable.
              this.error = errorText(err);
              break;
            }
            if (signal.aborted) return;
            const match = page.data.findIndex((turn) => retainedPositions.has(turn.id));
            if (match >= 0) {
              overlap = retainedPositions.get(page.data[match].id)!;
              newer.push(...page.data.slice(0, match + 1));
              break;
            }
            newer.push(...page.data);
            cursor = turnsBackwardsCursor =
              page.nextCursor && page.nextCursor !== cursor ? page.nextCursor : undefined;
          }
        }
        // ES2022 has no toReversed; the copy keeps the response in wire order.
        // oxlint-disable-next-line unicorn/no-array-reverse
        thread = { ...thread, turns: newer.reverse() };
      } else if (thread.turns?.length) {
        overlap = retained.findIndex((turn) => turn.id === thread.turns[0].id);
      }
      if (overlap >= 0) {
        thread = { ...thread, turns: [...retained.slice(0, overlap), ...thread.turns] };
        turnsBackwardsCursor = this.#turnsBackwardsCursor;
      }
      this.#turnsBackwardsCursor = turnsBackwardsCursor;
      this.#applyThread(thread, { model, effort, serviceTier, permissionProfile });
      const hidden = Math.max(0, this.main.order.length - transcriptPageSize);
      this.#hiddenEntries = overlap < 0 ? hidden : Math.min(hidden, this.#hiddenEntries);
      const subtreeRoot = this.family?.threads.includes(id)
        ? this.family.root
        : (thread.parentThreadId ?? id);
      const loadedMainProcesses = threadMayHaveProcesses(thread);
      if (loadedMainProcesses) void this.#loadProcesses(id, machine);
      else this.#clearProcesses(id);
      void this.#loadSubtree(subtreeRoot, machine, signal, {
        refreshLoaded: keepEarlier,
        skipThreadId: loadedMainProcesses ? id : undefined,
      });
      void this.#loadChanges(id, machine, signal);
      this.#loadRowReceipts(
        Math.max(this.#hiddenEntries, this.main.order.length - transcriptPageSize),
      );
    } catch (err) {
      if (signal.aborted) return;
      this.error = errorText(err);
      if (err instanceof TransportError && (err.status === 404 || err.status === 410)) {
        this.gone = { status: err.status, text: err.message };
      }
    } finally {
      if (catchupNotifications && this.#catchupNotifications === catchupNotifications) {
        this.#catchupNotifications = undefined;
        if (!signal.aborted && machine.connection === "open") {
          for (const notification of catchupNotifications) this.#onNotification(notification);
        }
      }
    }
  }

  #applyThread(
    thread: Thread,
    settings: Pick<Session, "model" | "effort" | "serviceTier" | "permissionProfile">,
  ): void {
    this.#workspaces.set(thread.id, thread.cwd);
    this.#turns = (thread.turns ?? []).map((t) => ({ ...t, items: [...t.items] }));
    this.#liveItemIds.clear();
    if (this.#hooks.some((h) => h.run.status === "running")) {
      this.#hooks = this.#hooks.filter((h) => h.run.status !== "running");
      if (this.#machine && this.#id) {
        this.#rememberHooks(`${this.#machine.id}:${this.#id}`, this.#hooks);
      }
    }
    let activeTurn: Turn | undefined;
    for (let i = this.#turns.length - 1; i >= 0; i--) {
      if (this.#turns[i].status === "inProgress") {
        activeTurn = this.#turns[i];
        break;
      }
    }
    this.activeTurnId = activeTurn?.id;
    const status = threadStatusToStatus(thread.status);
    if (activeTurn) status.running = true;
    this.status = status;
    this.input = { stoppable: !!activeTurn || !!status.running };
    this.session = {
      thread: thread.id,
      title: thread.name || thread.preview || undefined,
      workspace: thread.cwd || undefined,
      started: thread.createdAt ? thread.createdAt * 1000 : undefined,
      model: settings.model ?? thread.model ?? this.session?.model,
      effort: settings.effort ?? thread.reasoningEffort ?? this.session?.effort,
      serviceTier: settings.serviceTier ?? thread.serviceTier ?? this.session?.serviceTier,
      permissionProfile:
        settings.permissionProfile === undefined
          ? this.session?.permissionProfile
          : settings.permissionProfile,
    };
    this.#rebuildMain();
    if (!this.roster.agents.length) {
      this.roster = { total: 1, agents: [threadToAgent(thread, 0)] };
      this.#observeRoles();
    } else if (thread.tokenUsage || thread.extra?.skills?.length) {
      const usage = thread.tokenUsage ? usageFields(thread.tokenUsage) : undefined;
      this.roster = {
        ...this.roster,
        agents: this.roster.agents.map((agent) =>
          agent.thread === thread.id
            ? {
                ...agent,
                ...usage,
                skills: thread.extra?.skills?.length ? thread.extra.skills : agent.skills,
              }
            : agent,
        ),
      };
    }
    this.jumps++;
  }

  async #loadSubtree(
    id: string,
    machine: Machine,
    signal: AbortSignal,
    options: { refreshLoaded?: boolean; skipThreadId?: string } = {},
  ): Promise<void> {
    try {
      const response = await machine.request<ThreadListResponse>(
        "thread/subtree/list",
        { threadId: id },
        { signal },
      );
      if (signal.aborted) return;
      const threads = response.data ?? [];
      if (!threads.length) return;
      const root = threads.find((t) => !t.parentThreadId) ?? threads[0];
      const previous = new Set(this.family?.threads ?? [this.#id]);
      this.family = { root: root.id, threads: threads.map((t) => t.id) };
      this.#workspaces = new Map(threads.map((thread) => [thread.id, thread.cwd]));
      this.processes = this.processes.filter((process) => this.#inFamily(process.threadId));
      for (const thread of threads) {
        if (!threadMayHaveProcesses(thread)) {
          this.#clearProcesses(thread.id);
          continue;
        }
        if (
          thread.id !== options.skipThreadId &&
          (!previous.has(thread.id) || options.refreshLoaded)
        ) {
          void this.#loadProcesses(thread.id, machine);
        }
      }
      const prevByThread = new Map(
        this.roster.agents.filter((a) => a.thread).map((a) => [a.thread!, a]),
      );
      const agents = threads.map((t) => {
        const next = threadToAgent(t, t.id === root.id ? 0 : 1);
        const prev = prevByThread.get(t.id);
        if (!prev) return next;
        return {
          ...next,
          skills:
            prev.skills && next.skills
              ? [...new Set([...prev.skills, ...next.skills])]
              : (prev.skills ?? next.skills),
          inputTokens: prev.inputTokens ?? next.inputTokens,
          cachedInputTokens: prev.cachedInputTokens ?? next.cachedInputTokens,
          outputTokens: prev.outputTokens ?? next.outputTokens,
          reasoningOutputTokens: prev.reasoningOutputTokens ?? next.reasoningOutputTokens,
          usagePartial: prev.usagePartial ?? next.usagePartial,
          cost: prev.cost ?? next.cost,
          costPartial: prev.costPartial ?? next.costPartial,
          tps: prev.tps ?? next.tps,
          work: prev.work ?? next.work,
          workSince: prev.workSince ?? next.workSince,
          lastResponse: prev.lastResponse ?? next.lastResponse,
          lastCompleted: prev.lastCompleted ?? next.lastCompleted,
          turns: prev.turns ?? next.turns,
          roundtrips: prev.roundtrips ?? next.roundtrips,
          roundtripsPartial: prev.roundtripsPartial ?? next.roundtripsPartial,
          activity: prev.activity ?? next.activity,
          context: next.context ?? prev.context,
          window: next.window ?? prev.window,
        };
      });
      this.roster = { total: agents.length, agents };
      this.#observeRoles();
    } catch {
      // Subtree listing is optional for single-agent threads.
    }
  }

  #callerName(id: string): string {
    return this.roster.agents.find((agent) => agent.thread === id)?.name ?? id;
  }

  selectChangeCaller(caller: string): void {
    if (this.reviewCaller === caller || !this.#id || !this.#machine) return;
    this.reviewCaller = caller;
    void this.#loadChanges(this.#id, this.#machine, this.#requests.signal);
  }

  setIncludeExternalChanges(include: boolean): void {
    if (this.reviewIncludeExternal === include || !this.#id || !this.#machine) return;
    this.reviewIncludeExternal = include;
    void this.#loadChanges(this.#id, this.#machine, this.#requests.signal);
  }

  async moreChanges(demand?: AbortSignal): Promise<void> {
    const id = this.#id;
    const machine = this.#machine;
    const cursor = this.reviewCursor;
    const current = this.review;
    if (!id || !machine || !cursor || !current || this.reviewLoading) return;
    const revision = this.#reviewRevision;
    this.reviewLoading = true;
    this.reviewError = undefined;
    try {
      const signal = demand
        ? AbortSignal.any([demand, this.#requests.signal])
        : this.#requests.signal;
      const response = await machine.request<ThreadChangesReadResponse>(
        "thread/changes/read",
        {
          threadId: id,
          callerThreadId: this.reviewCaller || null,
          includeExternal: this.reviewIncludeExternal,
          limit: changesPageSize,
          cursor,
        },
        { signal },
      );
      if (signal.aborted || revision !== this.#reviewRevision) return;
      const page = changesToReview(response, revision, current.caller, (thread) =>
        this.#callerName(thread),
      );
      this.review = { ...current, files: [...current.files, ...page.files] };
      this.reviewCursor = response.nextCursor ?? undefined;
    } catch (error) {
      if (!demand?.aborted && revision === this.#reviewRevision)
        this.reviewError = errorText(error);
    } finally {
      if (revision === this.#reviewRevision) this.reviewLoading = false;
    }
  }

  retryChanges(): void {
    if (!this.#id || !this.#machine) return;
    if (this.reviewCursor) void this.moreChanges();
    else void this.#loadChanges(this.#id, this.#machine, this.#requests.signal);
  }

  async #loadChanges(id: string, machine: Machine, signal: AbortSignal): Promise<void> {
    const revision = ++this.#reviewRevision;
    const caller = this.reviewCaller;
    this.reviewLoading = true;
    this.reviewCursor = undefined;
    this.reviewError = undefined;
    try {
      const [response, overviewResponse] = await Promise.all([
        machine.request<ThreadChangesReadResponse>(
          "thread/changes/read",
          {
            threadId: id,
            callerThreadId: caller || null,
            includeExternal: this.reviewIncludeExternal,
            limit: changesPageSize,
          },
          { signal },
        ),
        caller
          ? machine
              .request<ThreadChangesReadResponse>(
                "thread/changes/read",
                {
                  threadId: id,
                  callerThreadId: null,
                  includeExternal: this.reviewIncludeExternal,
                  limit: 1,
                },
                { signal },
              )
              .catch(() => undefined)
          : Promise.resolve(undefined),
      ]);
      if (signal.aborted || revision !== this.#reviewRevision) return;
      const review = changesToReview(response, revision, caller, (thread) =>
        this.#callerName(thread),
      );
      if (!caller) {
        this.reviewOverview = {
          fileCount: review.fileCount,
          callers: review.callers,
          counts: review.counts,
        };
      } else if (overviewResponse) {
        const overview = changesToReview(overviewResponse, revision, "", (thread) =>
          this.#callerName(thread),
        );
        this.reviewOverview = {
          fileCount: overview.fileCount,
          callers: overview.callers,
          counts: overview.counts,
        };
      }
      this.review = review;
      this.reviewCursor = response.nextCursor ?? undefined;
    } catch (error) {
      if (!signal.aborted && revision === this.#reviewRevision) this.reviewError = errorText(error);
    } finally {
      if (!signal.aborted && revision === this.#reviewRevision) this.reviewLoading = false;
    }
  }

  // Saved Diff composes files across commands. The transcript instead needs the
  // original operation receipts, limited to this thread's newly shown command rows.
  #loadRowReceipts(start: number, end?: number): void {
    const id = this.#id;
    const machine = this.#machine;
    if (!id || machine?.connection !== "open") return;
    const commands: { turnId: string; itemId: string }[] = [];
    for (const seq of this.main.order.slice(start, end)) {
      const { turnId, itemId, running } = this.main.entries[seq];
      if (!turnId || !itemId || running || this.#editReceipts.has(previewId(id, turnId, itemId)))
        continue;
      const target = this.#deltaTargets.get(turnId)?.get(itemId);
      if (target?.items[target.index]?.type === "commandExecution")
        commands.push({ turnId, itemId });
    }
    if (commands.length) void this.#loadEditReceipts(id, machine, this.#requests.signal, commands);
  }

  async #loadEditReceipts(
    id: string,
    machine: Machine,
    signal: AbortSignal,
    commands: readonly { turnId: string; itemId: string }[],
    replace = false,
  ): Promise<void> {
    const keys = new Set(commands.map(({ turnId, itemId }) => previewId(id, turnId, itemId)));
    const receipts = new Map<string, { sequence: number; value: Value }[]>();
    const published = new Map<string, Value[]>();
    const seenCursors = new Set<string>();
    let cursor: string | undefined;
    try {
      do {
        const page: ThreadChangesReadResponse = await machine.request(
          "thread/changes/read",
          {
            threadId: id,
            receiptItemIds: commands.map(({ itemId }) => itemId),
            limit: receiptPageSize,
            cursor,
          },
          { signal },
        );
        if (signal.aborted) return;
        const origins = new Map(page.changes.map((origin) => [origin.changeId, origin]));
        const updated = new Set<string>();
        for (const file of page.files) {
          if ((file.scope && file.scope !== "workspace") || file.changeIds?.length !== 1) continue;
          const origin = origins.get(file.changeIds[0]);
          if (origin?.threadId !== id || !origin.turnId || !origin.itemId) continue;
          const key = previewId(id, origin.turnId, origin.itemId);
          if (!keys.has(key)) continue;
          let rows = receipts.get(key);
          if (!rows) receipts.set(key, (rows = []));
          rows.push({ sequence: origin.sequence, value: capturedEditValue(file) });
          updated.add(key);
        }
        for (const key of updated) {
          // A live completion's receipt wins over a concurrent hydration snapshot.
          if (!replace && this.#editReceipts.get(key) !== published.get(key)) continue;
          const rows = receipts.get(key)!;
          rows.sort((a, b) => a.sequence - b.sequence);
          const values = rows.map(({ value }) => value);
          this.#editReceipts.set(key, values);
          published.set(key, values);
        }
        cursor = page.nextCursor ?? undefined;
        if (cursor && seenCursors.has(cursor)) return;
        if (cursor) seenCursors.add(cursor);
      } while (cursor);
      for (const key of keys) {
        if (!receipts.has(key) && (replace || !this.#editReceipts.has(key)))
          this.#editReceipts.set(key, []);
      }
    } catch {
      // Unavailable receipt data does not replace the command or the Saved Diff state.
    }
  }

  #inFamily(id: string): boolean {
    return id === this.#id || !!this.family?.threads.includes(id);
  }

  #showsEdit(thread: string): boolean {
    return thread === this.#id || (this.#id === this.family?.root && this.#inFamily(thread));
  }

  #updatePreview(
    thread: string,
    turn: string,
    item: string,
    fields: Pick<Preview, "Files" | "Tool">,
    caller?: string,
  ): void {
    const key = previewId(thread, turn, item);
    const preview: Preview = {
      ID: key,
      Thread: thread,
      Turn: turn,
      Caller: caller ?? this.#callerName(thread),
      Workspace: this.#workspaces.get(thread) ?? "",
      Status: "",
      ...fields,
    };
    const previous = this.live.find((edit) => edit.ID === key);
    if (previous?.Complete || previous?.Settling) return;
    this.live = [...this.live.filter((edit) => edit.ID !== key), preview].slice(-16);
  }

  #updateShellSample(sample: CommandExecutionChangesUpdatedNotification): void {
    const { threadId, turnId, itemId, source, changes } = sample;
    const key = previewId(threadId, turnId, itemId);
    if (!changes.length && !this.live.some((edit) => edit.ID === key)) return;
    this.#updatePreview(
      threadId,
      turnId,
      itemId,
      { Files: changes.map(previewFile), Tool: "Shell" },
      source === "userShell" ? "User shell" : undefined,
    );
  }

  async #settlePreview(
    thread: string,
    turn: string,
    item: string,
    id: string,
    machine: Machine,
  ): Promise<void> {
    const key = previewId(thread, turn, item);
    const preview = this.live.find((edit) => edit.ID === key);
    if (!preview || preview.Complete || preview.Settling) return;
    this.live = this.live.map((edit) => (edit.ID === key ? { ...edit, Settling: true } : edit));
    const signal = this.#requests.signal;
    let savedPath: string | undefined;
    let savedFiles: ReviewFile[] | undefined;
    let externalSummary: string | undefined;
    let status = preview.Status;
    try {
      const workspaceReq = machine.request<ThreadChangesReadResponse>(
        "thread/changes/read",
        {
          threadId: thread,
          itemId: item,
          includeExternal: false,
          includeEphemeral: false,
          limit: 4,
        },
        { signal },
      );
      const allReq =
        preview.Tool === "Shell"
          ? machine
              .request<ThreadChangesReadResponse>(
                "thread/changes/read",
                {
                  threadId: thread,
                  itemId: item,
                  includeExternal: true,
                  includeEphemeral: true,
                  limit: 100,
                },
                { signal },
              )
              .catch(() => undefined)
          : Promise.resolve(undefined);
      const [workspaceResp, allResp] = await Promise.all([workspaceReq, allReq]);
      const matchesItem = (resp: ThreadChangesReadResponse) =>
        resp.changes.some(
          (origin) =>
            origin.threadId === thread && origin.turnId === turn && origin.itemId === item,
        );
      if (matchesItem(workspaceResp)) {
        const file = workspaceResp.files[0];
        savedPath = file?.absolutePath ?? file?.path;
        savedFiles = workspaceResp.files.map(changeFileReview);
        if (workspaceResp.nextCursor) status = "More in Saved Diff";
      } else if (preview.Tool === "Shell") {
        savedFiles = [];
      }
      if (allResp && matchesItem(allResp)) {
        const externalFiles = allResp.files.filter(
          (f) => f.scope === "external" || f.scope === "ephemeral",
        );
        if (externalFiles.length > 0) {
          const ids = [...new Set(externalFiles.flatMap((f) => f.changeIds ?? []))];
          const noun = externalFiles.length === 1 ? "external file" : "external files";
          externalSummary = ids.length
            ? `Captured ${externalFiles.length} ${noun} · mchanges ${ids.join(" ")} --summary`
            : `Captured ${externalFiles.length} ${noun}`;
        }
      }
    } catch {
      if (preview.Tool === "Shell") status = "Saved Diff could not be read";
    }
    if (signal.aborted) return;
    this.live = this.live.map((edit) =>
      edit.ID === key
        ? {
            ...edit,
            Complete: true,
            Settling: false,
            Files: savedFiles ?? edit.Files,
            SavedPath: savedPath,
            ExternalSummary: externalSummary,
            Status: status,
          }
        : edit,
    );
    if (savedPath) void this.#loadChanges(id, machine, signal);
  }

  #clearProcesses(threadId: string): void {
    this.#processReads.delete(threadId);
    if (this.processes.some((process) => process.threadId === threadId)) {
      this.processes = this.processes.filter((process) => process.threadId !== threadId);
    }
  }

  async #loadProcesses(threadId: string, machine: Machine, change?: ProcessChange): Promise<void> {
    const signal = this.#requests.signal;
    const revision = Symbol();
    this.#processReads.set(threadId, revision);
    const current = () =>
      !signal.aborted && this.#processReads.get(threadId) === revision && this.#inFamily(threadId);
    let processes: BackgroundTerminal[] = [];
    try {
      let request = change && processChangeReads.get(change);
      if (!request) {
        // One connector notification can reach several warm family views.
        // Share its read without letting any one view's eviction cancel it.
        request = machine.request<BackgroundTerminalsPage>(
          "thread/backgroundTerminals/list",
          { threadId },
          { signal: change ? undefined : signal },
        );
        if (change) processChangeReads.set(change, request);
      }
      const page = await request;
      processes = page.data.map((process) => ({ ...process, threadId }));
    } catch (error) {
      const rpcError = error instanceof TransportError ? error.rpcError : undefined;
      if (
        rpcError?.code !== -32600 ||
        rpcError.data?.reason !== "threadNotLoaded" ||
        rpcError.data.threadId !== threadId
      )
        return;
    }
    if (current()) {
      this.processes = [
        ...this.processes.filter((process) => process.threadId !== threadId),
        ...processes,
      ];
    }
  }

  async stopProcess(process: BackgroundTerminal): Promise<string | undefined> {
    const machine = this.#machine;
    if (!machine || !this.#inFamily(process.threadId)) return;
    const { threadId, processId, turnId, itemId } = process;
    try {
      const response = await machine.request<{ terminated: boolean }>(
        "thread/backgroundTerminals/terminate",
        { threadId, processId, invocation: { turnId, itemId } },
        { signal: this.#requests.signal },
      );
      if (machine === this.#machine && this.#inFamily(threadId))
        void this.#loadProcesses(threadId, machine);
      if (!response.terminated) return "The selected process is no longer running.";
    } catch (error) {
      return errorText(error, true);
    }
  }

  #onNotification(notification: ServerNotification): void {
    const id = this.#id;
    const machine = this.#machine;
    if (!id || !machine) return;
    // Terminal refreshes are independent of transcript catch-up. Do not replay
    // one after reconnect has already requested a fresh process list.
    if (notification.method === "thread/backgroundTerminals/changed") {
      if (this.#inFamily(notification.params.threadId))
        void this.#loadProcesses(notification.params.threadId, machine, notification);
      return;
    }
    if (
      this.#catchupNotifications &&
      "threadId" in notification.params &&
      notification.params.threadId === id
    ) {
      this.#catchupNotifications.push(notification);
      return;
    }
    switch (notification.method) {
      case "thread/started": {
        const started: Thread = notification.params.thread;
        const parent = started.parentThreadId;
        if (
          parent &&
          (parent === id || parent === this.family?.root || this.family?.threads.includes(parent))
        ) {
          const rootId = this.family?.root ?? parent;
          const threads = this.family?.threads ?? [rootId];
          const added = !threads.includes(started.id);
          if (added) {
            this.family = { root: rootId, threads: [...threads, started.id] };
            this.#workspaces.set(started.id, started.cwd);
            if (
              typeof started.extra?.backgroundTerminalCount === "number" &&
              started.extra.backgroundTerminalCount > 0
            ) {
              void this.#loadProcesses(started.id, machine);
            }
          }
          const nextAgent = threadToAgent(started, 1);
          const existingIdx = this.roster.agents.findIndex((a) => a.thread === started.id);
          const agents =
            existingIdx >= 0
              ? this.roster.agents.map((a, idx) => (idx === existingIdx ? nextAgent : a))
              : [...this.roster.agents, nextAgent];
          this.roster = { total: agents.length, agents };
          this.#observeRoles();
        }
        break;
      }
      case "thread/closed": {
        const { threadId } = notification.params;
        if (threadId === id) this.#subscribed = this.#maySubscribe = false;
        this.#processReads.delete(threadId);
        this.processes = this.processes.filter((process) => process.threadId !== threadId);
        break;
      }
      case "thread/archived":
      case "thread/deleted": {
        if (notification.params.threadId === id) this.#subscribed = false;
        break;
      }
      case "thread/status/changed": {
        const targetId = notification.params.threadId;
        if (notification.params.status.type === "notLoaded") this.#clearProcesses(targetId);
        if (targetId === id) {
          const status = threadStatusToStatus(notification.params.status);
          this.status = status;
          this.input = { ...this.input, stoppable: !!status.running || !!this.activeTurnId };
        }
        if (this.roster.agents.some((a) => a.thread === targetId)) {
          this.roster = {
            ...this.roster,
            agents: this.roster.agents.map((a) =>
              a.thread === targetId
                ? { ...a, state: threadStatusToAgentState(notification.params.status) }
                : a,
            ),
          };
        }
        break;
      }
      case "thread/name/updated": {
        if (notification.params.threadId !== id || !this.session) return;
        this.session = {
          ...this.session,
          title: notification.params.threadName || this.session.title,
        };
        break;
      }
      case "thread/settings/updated": {
        if (notification.params.threadId !== id || !this.session) return;
        const settings = notification.params.threadSettings;
        this.session = {
          ...this.session,
          model: settings.model || this.session.model,
          effort: settings.effort ?? undefined,
          serviceTier: settings.serviceTier ?? undefined,
          permissionProfile: settings.activePermissionProfile?.id ?? null,
        };
        break;
      }
      case "thread/tokenUsage/updated": {
        const targetId = notification.params.threadId;
        const matchesMain = targetId === id;
        const matchesRoster = this.roster.agents.some((a) => a.thread === targetId);
        if (!matchesMain && !matchesRoster) return;
        const usage = usageFields(notification.params.tokenUsage);
        this.roster = {
          ...this.roster,
          agents: this.roster.agents.map((a) =>
            a.thread === targetId || (matchesMain && !a.thread && a.name === "/root")
              ? {
                  ...a,
                  ...usage,
                  turns: usage.turns ?? a.turns,
                  context: usage.context ?? a.context,
                  window: usage.window ?? a.window,
                }
              : a,
          ),
        };
        break;
      }
      case "thread/skills/updated": {
        const { threadId, names } = notification.params;
        this.roster = {
          ...this.roster,
          agents: this.roster.agents.map((agent) =>
            agent.thread === threadId
              ? { ...agent, skills: [...new Set([...(agent.skills ?? []), ...names])] }
              : agent,
          ),
        };
        break;
      }
      case "turn/started": {
        const targetId = notification.params.threadId;
        const matchesMain = targetId === id;
        const matchesRoster = this.roster.agents.some((a) => a.thread === targetId);
        if (!matchesMain && !matchesRoster) return;
        const turn = notification.params.turn;
        this.live = this.live.filter((edit) => edit.Thread !== targetId || !edit.Complete);
        const startedAt = turn.startedAt == null ? undefined : turn.startedAt * 1000;
        if (matchesMain) {
          this.activeTurnId = turn.id;
          this.status = {
            ...this.status,
            running: true,
            text: "Working",
            failure: undefined,
            alert: false,
          };
          this.input = { ...this.input, stoppable: true };
          this.#upsertTurn(turn);
          this.#rebuildMain();
        }
        this.roster = {
          ...this.roster,
          agents: this.roster.agents.map((a) =>
            a.thread === targetId || (matchesMain && !a.thread && a.name === "/root")
              ? {
                  ...a,
                  state: "working",
                  workSince: startedAt,
                }
              : a,
          ),
        };
        break;
      }
      case "turn/completed": {
        const targetId = notification.params.threadId;
        const turn = notification.params.turn;
        const matchesMain = targetId === id;
        const matchesRoster = this.roster.agents.some((a) => a.thread === targetId);
        if (!matchesMain && !matchesRoster) return;
        this.live = this.live.map((edit) =>
          edit.Thread === targetId &&
          edit.Turn === turn.id &&
          edit.Tool !== "Shell" &&
          !edit.Settling
            ? { ...edit, Complete: true }
            : edit,
        );
        const elapsed =
          turn.durationMs != null
            ? Math.max(0, Number(turn.durationMs))
            : turn.startedAt != null && turn.completedAt != null
              ? Math.max(0, (turn.completedAt - turn.startedAt) * 1000)
              : undefined;
        const failed = turn.status === "failed";
        if (matchesMain) {
          if (this.activeTurnId === turn.id) this.activeTurnId = undefined;
          this.status = {
            running: false,
            alert: failed,
            completed: elapsed,
            failure: turn.error?.message,
            text: failed
              ? turn.error?.message
              : turn.status === "interrupted"
                ? "Interrupted"
                : undefined,
          };
          this.input = { ...this.input, stoppable: false };
          this.#upsertTurn(turn);
          this.#liveItemIds.clear();
          if (this.#hooks.some((h) => h.run.status === "running" && h.turnId === turn.id)) {
            this.#hooks = this.#hooks.filter(
              (h) => !(h.run.status === "running" && h.turnId === turn.id),
            );
            this.#rememberHooks(`${machine.id}:${id}`, this.#hooks);
          }
          this.#rebuildMain();
        }
        if (this.#showsEdit(targetId)) void this.#loadChanges(id, machine, this.#requests.signal);
        this.roster = {
          ...this.roster,
          agents: this.roster.agents.map((a) =>
            a.thread === targetId || (matchesMain && !a.thread && a.name === "/root")
              ? {
                  ...a,
                  state: failed ? "error" : "idle",
                  workSince: undefined,
                  activity: undefined,
                }
              : a,
          ),
        };
        break;
      }
      case "hook/started":
      case "hook/completed": {
        if (notification.params.threadId !== id) return;
        this.#upsertHook(
          notification.method === "hook/started",
          notification.params.turnId ?? null,
          notification.params.run,
        );
        this.#rebuildMain();
        break;
      }
      case "item/started":
      case "item/completed": {
        const targetId = notification.params.threadId;
        const started = notification.method === "item/started";
        const { item, turnId } = notification.params;
        if (this.#showsEdit(targetId)) {
          const key = previewId(targetId, turnId, item.id);
          if (
            !started &&
            ((item.type === "commandExecution" && item.status !== "inProgress") ||
              item.type === "fileChange")
          ) {
            if (item.type === "commandExecution") {
              const hasPreview = this.live.some((edit) => edit.ID === key);
              if (!hasPreview) {
                if (
                  item.source === "userShell" ||
                  item.commandActions?.some((action) => action.type === "unknown")
                ) {
                  void this.#loadChanges(id, machine, this.#requests.signal);
                }
              } else {
                void this.#settlePreview(targetId, turnId, item.id, id, machine);
              }
            } else {
              void this.#settlePreview(targetId, turnId, item.id, id, machine);
            }
          }
        }
        const agent = this.roster.agents.find((a) => a.thread === targetId);
        const activity = itemToEntry(item, 0, 0, turnId, agent?.name, started, targetId);
        if (!activity) return;
        if (targetId !== id) {
          if (agent) {
            this.roster = {
              ...this.roster,
              agents: this.roster.agents.map((a) =>
                a.thread === targetId ? { ...a, activity } : a,
              ),
            };
          }
          return;
        }
        if (started) {
          this.#liveItemIds.add(notification.params.item.id);
        } else {
          this.#liveItemIds.delete(notification.params.item.id);
        }
        this.#upsertItem(notification.params.turnId, notification.params.item);
        this.#rebuildMain();
        if (!started && item.type === "commandExecution" && item.status !== "inProgress")
          void this.#loadEditReceipts(
            id,
            machine,
            this.#requests.signal,
            [{ turnId, itemId: item.id }],
            true,
          );
        if (this.roster.agents.some((a) => a.thread === id || a.name === "/root")) {
          this.roster = {
            ...this.roster,
            agents: this.roster.agents.map((a) =>
              a.thread === id || (!a.thread && a.name === "/root") ? { ...a, activity } : a,
            ),
          };
        }
        if (
          notification.params.item.type === "collabAgentToolCall" &&
          (notification.params.item.tool === "spawnAgent" ||
            notification.params.item.tool === "resumeAgent" ||
            notification.params.item.tool === "closeAgent")
        ) {
          void this.#loadSubtree(this.family?.root ?? id, machine, this.#requests.signal);
        }
        break;
      }
      case "item/agentMessage/delta": {
        if (notification.params.threadId !== id) return;
        this.#appendItemDelta(notification.params.turnId, notification.params.itemId, (item) => {
          if (item.type === "agentMessage")
            return { ...item, text: item.text + notification.params.delta };
          return item;
        });
        break;
      }
      case "item/plan/delta": {
        if (notification.params.threadId !== id) return;
        this.#appendItemDelta(notification.params.turnId, notification.params.itemId, (item) => {
          if (item.type === "plan") return { ...item, text: item.text + notification.params.delta };
          return item;
        });
        break;
      }
      case "item/commandExecution/outputDelta": {
        if (notification.params.threadId !== id) return;
        this.#appendItemDelta(notification.params.turnId, notification.params.itemId, (item) => {
          if (item.type === "commandExecution")
            return {
              ...item,
              aggregatedOutput: (item.aggregatedOutput ?? "") + notification.params.delta,
            };
          return item;
        });
        break;
      }
      case "item/commandExecution/changesUpdated": {
        const { threadId } = notification.params;
        if (!this.#showsEdit(threadId)) return;
        this.#updateShellSample(notification.params);
        break;
      }
      case "item/reasoning/summaryTextDelta": {
        if (notification.params.threadId !== id) return;
        this.#appendItemDelta(notification.params.turnId, notification.params.itemId, (item) => {
          if (item.type !== "reasoning") return item;
          const summary = [...item.summary];
          const idx = notification.params.summaryIndex;
          while (summary.length <= idx) summary.push("");
          summary[idx] += notification.params.delta;
          return { ...item, summary };
        });
        break;
      }
      case "item/fileChange/patchUpdated": {
        const { threadId, turnId, itemId, changes } = notification.params;
        if (!this.#showsEdit(threadId)) return;
        if (threadId === id && turnId !== this.activeTurnId && !this.#liveItemIds.has(itemId))
          return;
        const item = this.#turns
          .find((turn) => turn.id === turnId)
          ?.items.find((entry) => entry.id === itemId);
        if (
          (item?.type === "fileChange" || item?.type === "commandExecution") &&
          item.status !== "inProgress"
        )
          return;
        if (!changes.length) {
          const key = previewId(threadId, turnId, itemId);
          const previous = this.live.find((edit) => edit.ID === key);
          if (!previous || previous.Complete || previous.Settling) return;
          this.live = this.live.map((edit) =>
            edit.ID === key ? { ...edit, Files: [], Withdrawn: true } : edit,
          );
          return;
        }
        this.#updatePreview(threadId, turnId, itemId, {
          Tool: "Edit",
          Files: changes.map((change) => ({
            ...previewFile(change),
            Diff: nativePatchDiff(change.kind.type, change.diff),
          })),
        });
        break;
      }
      case "turn/diff/updated": {
        if (!this.#showsEdit(notification.params.threadId)) return;
        void this.#loadChanges(id, machine, this.#requests.signal);
        break;
      }
      case "thread/reverted": {
        if (notification.params.threadId !== id) return;
        this.live = [];
        // A revert can arrive while the previous snapshot is still in flight.
        const pending = this.#hydration;
        const signal = this.#requests.signal;
        if (!pending) void this.refresh(false);
        else
          void pending.then(() => {
            if (!signal.aborted) void this.refresh(false);
          });
        break;
      }
      default:
        break;
    }
  }

  #rebuildMain(): void {
    const first = this.#firstOwnTurn
      ? this.#turns.findIndex((turn) => turn.id === this.#firstOwnTurn)
      : -1;
    if (first >= 0) {
      this.#turns = this.#turns.slice(first);
      this.#turnsBackwardsCursor = undefined;
    }
    this.#deltaTargets.clear();
    const main = buildFeed(
      this.#turns,
      this.#id,
      this.#liveItemIds,
      this.#hooks,
      (turn, index, entry) => {
        let items = this.#deltaTargets.get(turn.id);
        if (!items) this.#deltaTargets.set(turn.id, (items = new Map()));
        items.set(turn.items[index].id, { items: turn.items, index, seq: entry.seq });
      },
    );
    this.#entries = new SvelteMap(main.order.map((seq) => [seq, main.entries[seq]]));
    this.main = main;
  }

  showFrom(threadId: string, turnId: string): void {
    if (threadId !== this.#id || this.#firstOwnTurn === turnId) return;
    this.#firstOwnTurn = turnId;
    this.#hiddenEntries = 0;
    this.#rebuildMain();
  }

  #upsertHook(started: boolean, turnId: string | null, run: HookRunSummary): void {
    let matched = false;
    if (!started) {
      for (let i = this.#hooks.length - 1; i >= 0; i--) {
        const candidate = this.#hooks[i];
        if (
          candidate.id === run.id &&
          candidate.run.status === "running" &&
          (!turnId || !candidate.turnId || candidate.turnId === turnId)
        ) {
          candidate.run = run;
          if (turnId) candidate.turnId = turnId;
          matched = true;
          break;
        }
      }
    }
    if (!matched) {
      const turn = turnId ? this.#turns.find((t) => t.id === turnId) : undefined;
      this.#hooks.push({
        key: `${run.id}:${++this.#hookSeq}`,
        id: run.id,
        turnId,
        afterItemCount: turn ? turn.items.length : 0,
        run,
      });
    }
    this.#hooks = trimHooks(this.#hooks);
    if (this.#machine && this.#id) {
      this.#rememberHooks(`${this.#machine.id}:${this.#id}`, this.#hooks);
    }
  }

  #rememberHooks(cacheKey: string, hooks: HookRecord[]): void {
    this.#hooksByThread.delete(cacheKey);
    if (!hooks.length) return;
    this.#hooksByThread.set(cacheKey, { records: hooks, sequence: this.#hookSeq });
    while (this.#hooksByThread.size > maxCachedHookThreads) {
      const oldest = this.#hooksByThread.keys().next().value;
      if (oldest === undefined) break;
      this.#hooksByThread.delete(oldest);
    }
  }

  #upsertTurn(turn: Turn): void {
    const index = this.#turns.findIndex((t) => t.id === turn.id);
    if (index >= 0) {
      const existing = this.#turns[index];
      let items: ThreadItem[];
      if (turn.itemsView === "full" && turn.items.length) {
        items = [...turn.items];
      } else if (turn.items.length) {
        items = [...existing.items];
        for (const item of turn.items) {
          const itemIdx = items.findIndex((e) => e.id === item.id);
          if (itemIdx >= 0) items[itemIdx] = item;
          else items.push(item);
        }
      } else {
        items = existing.items;
      }
      this.#turns[index] = {
        ...turn,
        items,
        itemsView: existing.itemsView === "full" ? "full" : turn.itemsView,
      };
    } else {
      this.#turns.push({ ...turn, items: [...turn.items] });
    }
  }

  #upsertItem(turnId: string, item: ThreadItem): void {
    let turn = this.#turns.find((t) => t.id === turnId);
    if (!turn) {
      const created: Turn = {
        id: turnId,
        rootTurnId: null,
        items: [],
        itemsView: "full",
        status: "inProgress",
        error: null,
        startedAt: null,
        completedAt: null,
        durationMs: null,
      };
      this.#turns.push(created);
      turn = created;
    }
    const index = turn.items.findIndex((existing) => existing.id === item.id);
    if (index >= 0) turn.items[index] = item;
    else turn.items.push(item);
  }

  #appendItemDelta(turnId: string, itemId: string, update: (item: ThreadItem) => ThreadItem): void {
    const target = this.#deltaTargets.get(turnId)?.get(itemId);
    if (!target) return;
    const { items, index, seq } = target;
    const previous = items[index];
    const item = update(previous);
    if (item === previous) return;
    items[index] = item;
    const entry = itemToEntry(
      item,
      seq,
      0,
      turnId,
      "/root",
      this.#liveItemIds.has(itemId),
      this.#id,
    );
    if (entry) this.#entries.set(seq, entry);
  }

  async close(): Promise<void> {
    const prevId = this.#id;
    const prevMachine = this.#machine;
    const resumeRequest = this.#resumeRequest;
    const predecessor = this.#predecessor;
    const maySubscribe = this.#maySubscribe;
    this.#id = undefined;
    this.#machine = undefined;
    this.#liveItemIds.clear();
    this.#hooks = [];
    this.#stopNotifications?.();
    this.#stopNotifications = undefined;
    this.#stopOpen?.();
    this.#stopOpen = undefined;
    this.#requests.abort();
    this.#processReads.clear();
    this.processes = [];
    if (predecessor) await predecessor;
    if (resumeRequest) await resumeRequest.catch(() => {});
    if (maySubscribe && prevId && prevMachine?.connection === "open") {
      await prevMachine.request("thread/unsubscribe", { threadId: prevId }).catch(() => {});
    }
  }

  #observeRoles(): void {
    const seen = this.roles;
    const added = this.roster.agents
      .map((agent) => agent.role)
      .filter((role, i, all): role is string => !!role && all.indexOf(role) === i)
      .filter((role) => !seen.includes(role));
    if (added.length) this.roles = [...seen, ...added];
  }

  roleColor(role: string | undefined): string | undefined {
    const index = role ? this.roles.indexOf(role) : -1;
    return index < 0 ? undefined : roleColor(index);
  }

  label(name: string | undefined): string {
    if (!name || name === "/root") return "main";
    return (this.identities.get(name)?.label ?? name).replace(/^\/root\//, "");
  }

  agent(name: string | undefined): Agent | undefined {
    return name ? this.byName.get(this.address(name)) : undefined;
  }

  address(name: string): string {
    return this.identities.get(name)?.name ?? name;
  }

  async readContent(ref: ContentReference, signal?: AbortSignal): Promise<ContentResourceResponse> {
    const machine = this.#machine;
    const id = this.#id;
    if (!machine || !id) throw new Error("Open a thread on an execution machine first.");
    const lifetime = signal
      ? AbortSignal.any([signal, this.#requests.signal])
      : this.#requests.signal;
    return machine.resource<ContentResourceResponse>(
      "content",
      { thread: id, body: { thread: id, turnId: ref.turnId, itemId: ref.itemId } },
      { signal: lifetime },
    );
  }

  async image(
    entry: Pick<Entry, "itemId" | "seq" | "agent">,
    index: number,
    image: string,
    signal: AbortSignal,
  ): Promise<Blob> {
    const machine = this.#machine;
    const id = this.#id;
    if (!machine || !id) throw new Error("The image's machine is disconnected.");
    // http_resources.rs matches body.image against SHA-256 hex digest or "".
    // When itemId is present, omit raw file paths/URLs so lookup matches by itemId + index.
    const identity = /^[0-9a-f]{64}$/i.test(image) ? image : "";
    return machine.resource<Blob>(
      "image",
      { thread: id, body: { itemId: entry.itemId, index, image: identity } },
      { signal: AbortSignal.any([signal, this.#requests.signal]) },
    );
  }
}

const maxWatchedThreads = 6;
const warmIdleMs = 5 * 60_000;

export class ThreadStreams {
  #hooks: HookCache = new Map();
  current = $state.raw(new Stream(this.#hooks));
  #selected?: Stream;
  #watched = new Map<Stream, ReturnType<typeof setTimeout> | undefined>();
  #retiring = new WeakMap<Machine, Map<string, Promise<void>>>();

  connect(
    id: string,
    machine: Machine,
    started?: InitialInput["started"],
    firstOwnTurn?: string,
  ): void {
    const previous = this.current;
    if (this.#selected === previous && previous.threadId === id && previous.machine === machine)
      return;
    if (this.#selected) this.release(this.#selected.threadId, this.#selected.machine);
    let next = [...this.#watched.keys()].find(
      (thread) => thread.threadId === id && thread.machine === machine,
    );
    if (next) {
      clearTimeout(this.#watched.get(next));
      this.#watched.delete(next);
      next.setSelected(true);
      if (firstOwnTurn) next.showFrom(id, firstOwnTurn);
    } else {
      next = new Stream(this.#hooks);
      const predecessor = this.#retiring.get(machine)?.get(id);
      next.connect(id, machine, { started, firstOwnTurn, familySource: previous, predecessor });
    }
    this.current = this.#selected = next;
    this.#watched.set(next, undefined);
    if (this.#watched.size > maxWatchedThreads) {
      const oldest = this.#watched.keys().next().value;
      if (oldest) this.#evict(oldest);
    }
  }

  release(id: string | undefined, machine: Machine | undefined): void {
    const thread = this.#selected;
    if (!thread || thread.threadId !== id || thread.machine !== machine) return;
    this.#selected = undefined;
    thread.setSelected(false);
    this.#watched.set(
      thread,
      setTimeout(() => this.#evict(thread), warmIdleMs),
    );
  }

  #evict(thread: Stream): void {
    clearTimeout(this.#watched.get(thread));
    this.#watched.delete(thread);
    const { threadId: id, machine } = thread;
    if (!id || !machine) return;
    if (this.current === thread) this.current = new Stream(this.#hooks);
    let retiring = this.#retiring.get(machine);
    if (!retiring) this.#retiring.set(machine, (retiring = new Map()));
    const done = thread.close();
    machine.retireThreadPrompts(id, done);
    retiring.set(id, done);
    void done.then(() => {
      if (retiring.get(id) === done) retiring.delete(id);
    });
  }
}

export const streams = new ThreadStreams();
