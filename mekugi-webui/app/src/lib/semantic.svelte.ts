import type { Machine } from "./hub.svelte";
import { errorText, TransportError, type Connection } from "./transport";
import { streams } from "./stream.svelte";
import { SideAnswer } from "./side-answer.svelte";
import {
  attachmentOffsets,
  formatPathMention,
  prepareAttachments,
  selectionAppendix,
  textUploadFrames,
  textUploadSnapshot,
  uploadBytes,
  uploadLimit,
  type PendingAttachment,
} from "./attachments";
import type {
  Approval,
  ApprovalChoice,
  Attachment,
  CommandExecutionRequestApprovalResponse,
  FileChangeRequestApprovalResponse,
  Prompts,
  Question,
  QuestionCall,
  QueuedSubmission,
  ServerNotification,
  ServerRequest,
  ShellGuardRequestApprovalResponse,
  ThreadForkResponse,
  ThreadPredictionRequestResponse,
  ThreadQueueAddParams,
  ThreadQueueListResponse,
  ThreadRevertResponse,
  ThreadSetNameParams,
  ThreadSettingsUpdateParams,
  ToolRequestUserInputAnswer,
  ToolRequestUserInputResponse,
  TurnInterruptParams,
  TurnStartParams,
  TurnStartResponse,
  TurnSteerParams,
  UserInput,
} from "./protocol";

const encoder = new TextEncoder();

export type Editor = {
  text: string;
  items: PendingAttachment[];
  skills: SelectedSkill[];
  revision: number;
  submission?: AbortController;
};

export type SelectedSkill = { start: number; name: string; path: string };

export const SLASH_COMMANDS = [
  { name: "/compact", description: "Compact context now; Tab queues while busy" },
  { name: "/ps", description: "Show active processes" },
  { name: "/btw", description: "Ask a side question", argument: "QUESTION" },
  { name: "/oneoff", description: "Run a task in a separate conversation", argument: "TASK" },
  { name: "/rewind", description: "Rewind before a user message" },
  { name: "/archive", description: "Archive this thread" },
] as const;

type DraftAttachments = {
  count: number;
  read: () => Promise<Attachment[]>;
};

type SlashActions = {
  archive: () => void;
  rewind: () => string | undefined;
  oneOff: (
    task: string,
    readAttachments: DraftAttachments["read"],
    skills?: SelectedSkill[],
  ) => Promise<string | undefined>;
};

export type RewindPreview = { changeIds: string[]; reason: string | null };

export type RecoveryDraft = {
  id: string;
  clientUserMessageId?: string;
  input: UserInput[];
  shell?: string;
  queued: boolean;
  editing: boolean;
  error?: string;
  responsesapiClientMetadata?: QueuedSubmission["responsesapiClientMetadata"];
};

export function editableRecoveryPart(
  part: UserInput,
): part is Extract<UserInput, { type: "text" }> {
  return (
    part.type === "text" &&
    !part.text.startsWith(selectionAppendix) &&
    !textUploadFrames(part.text).length
  );
}

function reviseRecoveryInput(input: UserInput[], edits: string[]): UserInput[] {
  let index = 0;
  return input.map((part) => {
    if (!editableRecoveryPart(part)) return part;
    const text = edits[index++] ?? part.text;
    if (text === part.text) return part;
    return {
      ...part,
      text,
      text_elements: part.text_elements?.flatMap(({ placeholder }) => {
        if (!placeholder) return [];
        const position = text.indexOf(placeholder);
        if (position < 0 || text.indexOf(placeholder, position + 1) >= 0) return [];
        const start = encoder.encode(text.slice(0, position)).length;
        return [
          { placeholder, byteRange: { start, end: start + encoder.encode(placeholder).length } },
        ];
      }),
    };
  });
}

// Keep an explicit skill selection only while its original token survives
// the edit. Manually typed skill names use the native resolver instead.
export function skillsAfterEdit(
  before: string,
  after: string,
  skills: SelectedSkill[],
): SelectedSkill[] {
  if (before === after || !skills.length) return skills;
  let start = 0;
  while (start < before.length && before[start] === after[start]) start++;
  let oldEnd = before.length;
  let newEnd = after.length;
  while (oldEnd > start && newEnd > start && before[oldEnd - 1] === after[newEnd - 1]) {
    oldEnd--;
    newEnd--;
  }
  return skills.flatMap((skill) => {
    const end = skill.start + skill.name.length + 1;
    if (end > start && skill.start < oldEnd) return [];
    const offset = end <= start ? skill.start : skill.start + newEnd - oldEnd;
    const token = "$" + skill.name;
    return after.startsWith(token, offset) &&
      (offset === 0 || /\s/.test(after[offset - 1])) &&
      !/[\w:-]/.test(after[offset + token.length] ?? "")
      ? [{ ...skill, start: offset }]
      : [];
  });
}

export type QuestionDraft = {
  choice: number | "skip" | undefined;
  text: string;
  note: string;
  items: PendingAttachment[];
  sending: boolean;
  accepted: boolean;
};

type QuestionAnswer = {
  question: string;
  choice?: number;
  text?: string;
  note?: string;
  skip?: boolean;
  items?: PendingAttachment[];
};

export type PromptState = {
  drafts: Record<string, QuestionDraft>;
  deny?: number;
  reason: string;
  sending: boolean;
  accepted: boolean;
  error?: string;
  resolved?: boolean;
};

type PromptLifetime = { viewId: string; draft?: PromptState };

export function questionDraft(question: Question): QuestionDraft {
  const recommended = question.options?.findIndex((option) => option.recommended) ?? -1;
  return {
    choice: recommended >= 0 ? recommended : question.options?.length ? undefined : 0,
    text: "",
    note: "",
    items: [],
    sending: false,
    accepted: false,
  };
}

export function questionAnswers(
  call: QuestionCall,
  drafts: Record<string, QuestionDraft>,
  id?: string,
): QuestionAnswer[] {
  const questions = call.blocking ? call.questions : call.questions.filter((q) => q.id === id);
  if (!questions.length) throw new Error("Choose a question to answer.");
  let bytes = 0;
  return questions.map((q) => {
    const draft = drafts[q.id];
    const written = draft?.choice === (q.options?.length ?? 0);
    const items = written && call.blocking ? draft.items : [];
    if (!draft || draft.choice === undefined || (written && !draft.text.trim() && !items.length)) {
      throw new Error(
        call.blocking
          ? "Answer every question or skip it."
          : "Choose an option, write an answer or skip this question.",
        { cause: q.id },
      );
    }
    bytes += uploadBytes(items);
    if (bytes > uploadLimit)
      throw new Error("Uploads exceed the 32 MiB limit. Remove images and try again.", {
        cause: q.id,
      });
    return {
      question: q.id,
      choice: typeof draft.choice === "number" && !written ? draft.choice + 1 : undefined,
      text: written ? draft.text : "",
      note: typeof draft.choice === "number" && !written ? draft.note : "",
      skip: draft.choice === "skip",
      ...(items.length ? { items } : {}),
    };
  });
}

function inferImageMime(name: string): string {
  const ext = name.split(".").pop()?.toLowerCase();
  switch (ext) {
    case "png":
      return "image/png";
    case "jpg":
    case "jpeg":
      return "image/jpeg";
    case "gif":
      return "image/gif";
    case "webp":
      return "image/webp";
    case "svg":
      return "image/svg+xml";
    case "bmp":
      return "image/bmp";
    case "avif":
      return "image/avif";
    default:
      return "application/octet-stream";
  }
}

function newClientMessageId(): string {
  return (
    globalThis.crypto?.randomUUID?.() ??
    `msg-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`
  );
}

export function formatShellArg(arg: string): string {
  if (arg === "") return "''";
  if (/^[A-Za-z0-9_./:@%+=,-]+$/.test(arg)) return arg;
  for (let i = 0; i < arg.length; i++) {
    const code = arg.charCodeAt(i);
    if (code < 0x20 || code === 0x7f) return JSON.stringify(arg);
  }
  return `'${arg.replaceAll("'", "'\\''")}'`;
}

export function toNativeUserInput(
  text: string,
  attachments: Attachment[] = [],
  skills: SelectedSkill[] = [],
): UserInput[] {
  const items: UserInput[] = [];
  const selections: string[] = [];
  if (text.trim()) {
    const elements = attachments.flatMap((attachment) => {
      const placeholder =
        attachment.placeholder ??
        (attachment.source === "selection" && attachment.inline
          ? `[Selected ${attachment.name}]`
          : undefined);
      if (!placeholder) return [];
      const [offset] = attachmentOffsets(text, placeholder);
      if (offset === undefined) return [];
      const start = encoder.encode(text.slice(0, offset)).length;
      const end = start + encoder.encode(placeholder).length;
      return [{ byteRange: { start, end }, placeholder }];
    });
    // ES2022 has no toSorted; elements is owned by this projection.
    // oxlint-disable-next-line unicorn/no-array-sort
    elements.sort((a, b) => a.byteRange.start - b.byteRange.start);
    items.push({ type: "text", text, text_elements: elements });
  }
  for (const att of attachments) {
    if (att.source === "path") {
      if (att.fileReference) {
        const mention = formatPathMention(att.path);
        if (att.placeholder !== mention || !attachmentOffsets(text, mention).length) {
          items.push({ type: "text", text: mention, text_elements: [] });
        }
      } else {
        items.push({ type: "localImage", path: att.path });
      }
    } else if (att.source === "upload") {
      const url = att.data.startsWith("data:")
        ? att.data
        : `data:${att.mime || inferImageMime(att.name)};base64,${att.data}`;
      items.push({ type: "image", url });
    } else if (att.source === "selection") {
      const label = att.placeholder ?? `[Selected ${att.name}]`;
      if (!text.includes(label)) continue;
      let fenceLength = 3;
      for (const [run] of att.data.matchAll(/`+/g))
        fenceLength = Math.max(fenceLength, run.length + 1);
      const fence = "`".repeat(fenceLength);
      const origin = att.path ? ` (${att.path})` : "";
      selections.push(
        `- ${label.slice(10, -1)}${origin}:\n  ${fence}\n  ${att.data.replaceAll("\n", "\n  ")}\n  ${fence}`,
      );
    }
  }
  if (selections.length) {
    items.push({
      type: "text",
      text: selectionAppendix + selections.join("\n"),
      text_elements: [],
    });
  }
  if (attachments.some((item) => item.source === "textUpload")) {
    const snapshot = textUploadSnapshot(attachments, newClientMessageId());
    if (snapshot) items.push({ type: "text", text: snapshot, text_elements: [] });
  }
  for (const { name, path } of skills) items.push({ type: "skill", name, path });
  return items;
}

function approvalChoices(remember: boolean, denyWithReason = false): ApprovalChoice[] {
  const choices: ApprovalChoice[] = [{ id: 0, label: "Allow once", decision: "accept" }];
  if (remember) choices.push({ id: 1, label: "Allow for session", decision: "acceptForSession" });
  choices.push({
    id: choices.length,
    label: denyWithReason ? "Deny" : "Decline",
    decision: "decline",
    deny: denyWithReason,
  });
  return choices;
}

const KNOWN_SERVER_REQUEST_METHODS = new Set<string>([
  "item/commandExecution/requestApproval",
  "item/fileChange/requestApproval",
  // Local terminal handles permission and secret prompts; remote web clients
  // must ignore them rather than replying with an error that denies them.
  "item/permissions/requestApproval",
  "shellGuard/requestApproval",
  "item/tool/requestUserInput",
  "shellGuard/requestSecret",
]);

export function serverRequestToPrompt(
  req: ServerRequest,
  threadId: string,
): { kind: "approval"; approval: Approval } | { kind: "question"; call: QuestionCall } | undefined {
  switch (req.method) {
    case "item/commandExecution/requestApproval": {
      if (req.params.threadId !== threadId) return undefined;
      const details: string[] = [];
      if (req.params.cwd) details.push(`Directory: ${req.params.cwd}`);
      if (req.params.networkApprovalContext)
        details.push(`Network host: ${req.params.networkApprovalContext.host}`);
      if (req.params.reason) details.push(req.params.reason);
      return {
        kind: "approval",
        approval: {
          id: String(req.id),
          requestId: req.id,
          method: req.method,
          threadId,
          title: "Approve command execution",
          subject: req.params.command ?? undefined,
          details,
          choices: approvalChoices(
            req.params.kind === "command" &&
              req.params.networkApprovalContext == null &&
              req.params.additionalPermissions == null &&
              req.params.availableDecisions?.includes("acceptForSession") === true,
          ),
        },
      };
    }
    case "item/fileChange/requestApproval": {
      if (req.params.threadId !== threadId) return undefined;
      const details: string[] = [];
      if (req.params.reason) details.push(req.params.reason);
      return {
        kind: "approval",
        approval: {
          id: String(req.id),
          requestId: req.id,
          method: req.method,
          threadId,
          title: "Approve file change",
          subject: req.params.grantRoot ?? undefined,
          details,
          choices: approvalChoices(req.params.reason == null && req.params.grantRoot == null),
        },
      };
    }
    case "shellGuard/requestApproval": {
      if (req.params.threadId !== threadId) return undefined;
      const command = [req.params.executable, ...req.params.argv].map(formatShellArg).join(" ");
      return {
        kind: "approval",
        approval: {
          id: String(req.id),
          requestId: req.id,
          method: req.method,
          threadId,
          title: "Approve guarded shell action",
          subject: command,
          details: req.params.cwd ? [`Directory: ${req.params.cwd}`] : [],
          choices: approvalChoices(req.params.kind === "remoteWrite", true),
        },
      };
    }
    case "item/tool/requestUserInput": {
      if (req.params.threadId !== threadId) return undefined;
      const hasSecret = req.params.questions.some((q) => q.isSecret);
      return {
        kind: "question",
        call: {
          id: String(req.id),
          requestId: req.id,
          threadId,
          blocking: req.params.isBlocking,
          secretBlocked: hasSecret,
          questions: req.params.questions.map((q) => ({
            id: q.id,
            header: q.header ?? undefined,
            question: q.question,
            text: q.question,
            isOther: q.isOther,
            isSecret: q.isSecret,
            secret: q.isSecret,
            options: q.options?.map((o) => ({
              label: o.label,
              description: o.description ?? undefined,
              preview: o.preview ?? undefined,
              recommended: o.recommended,
            })),
          })),
        },
      };
    }
    default:
      // shellGuard/requestSecret and other local-only requests are never answered remotely.
      return undefined;
  }
}

export class ThreadControl {
  #id?: string;
  #main?: string;
  #machine?: Machine;
  #stopRequests?: () => void;
  #stopNotifications?: () => void;
  #stopOpen?: () => void;
  #queueRead = 0;
  #editors = new Map<string, Editor>();
  #nextPromptView = 0;
  #promptLifetimes = new WeakMap<ServerRequest, PromptLifetime>();
  #visiblePrompts = new Map<string, PromptLifetime>();
  #recoveryByMachine = new WeakMap<Machine, Map<string, RecoveryDraft[]>>();
  #answersByMachine = new WeakMap<Machine, Map<string, SideAnswer>>();
  #predictionRequest?: { threadId: string; sourceTurnId: string };

  pending = $state(0);
  error = $state<string | undefined>();
  prompts = $state.raw<Prompts>({ approvals: [], questions: [] });
  queued = $state.raw<QueuedSubmission[]>([]);
  recoveries = $state.raw<RecoveryDraft[]>([]);
  sideAnswer = $state.raw<SideAnswer>();
  predictionsEnabled = $state(false);
  prediction = $state<string>();

  get connection(): Connection {
    return this.#machine?.connection ?? "connecting";
  }

  get threadId(): string | undefined {
    return this.#id;
  }

  get machine(): Machine | undefined {
    return this.#machine;
  }

  connect(id: string, machine: Machine, main = id): void {
    if (this.#id === id && this.#main === main && this.#machine === machine && this.#stopRequests)
      return;
    this.release();
    this.#id = id;
    this.#main = main;
    this.#machine = machine;
    this.sideAnswer = this.#answersByMachine.get(machine)?.get(main);
    this.recoveries = this.#recoveryByMachine.get(machine)?.get(id) ?? [];
    this.error = undefined;
    this.queued = [];
    this.#stopNotifications = machine.onNotification((n) => this.#onNotification(n));
    this.#listenForPrompts(machine);
    this.#stopOpen = machine.onOpen(() => {
      this.#listenForPrompts(machine);
      void this.#refreshQueue();
    });
    void this.#refreshQueue();
  }

  release(id?: string, machine?: Machine): void {
    if (id && this.#id !== id) return;
    if (machine && this.#machine !== machine) return;
    this.#stopRequests?.();
    this.#stopRequests = undefined;
    this.#stopNotifications?.();
    this.#stopNotifications = undefined;
    this.#stopOpen?.();
    this.#stopOpen = undefined;
    this.#id = undefined;
    this.#main = undefined;
    this.#machine = undefined;
    this.#clearPrediction();
    this.sideAnswer = undefined;
    this.#clearPrompts();
    this.queued = [];
    this.recoveries = [];
    ++this.#queueRead;
    this.error = undefined;
  }

  editor(key = this.#id ?? "default"): Editor {
    let entry = this.#editors.get(key);
    if (!entry) {
      entry = { text: "", items: [], skills: [], revision: 0 };
      this.#editors.set(key, entry);
    }
    return entry;
  }

  prompt(id: string): PromptState {
    const lifetime = this.#visiblePrompts.get(id);
    if (lifetime?.draft) return lifetime.draft;
    const draft = $state<PromptState>({
      drafts: {},
      reason: "",
      sending: false,
      accepted: false,
    });
    if (lifetime) lifetime.draft = draft;
    return draft;
  }

  #clearPrompts(): void {
    this.prompts = { approvals: [], questions: [] };
    this.#visiblePrompts.clear();
  }

  #listenForPrompts(machine: Machine): void {
    this.#stopRequests?.();
    this.#clearPrompts();
    this.#stopRequests = machine.onServerRequest((req) => this.#onServerRequest(req));
  }

  #removePrompt(id: string): void {
    this.#visiblePrompts.delete(id);
    this.prompts = {
      approvals: this.prompts.approvals.filter((a) => a.id !== id),
      questions: this.prompts.questions.filter((q) => q.id !== id),
    };
  }

  #onServerRequest(req: ServerRequest): void {
    if (!KNOWN_SERVER_REQUEST_METHODS.has(req.method)) {
      this.#machine?.respondError(
        req.id,
        -32601,
        `unsupported web client server request: ${req.method}`,
      );
      return;
    }
    if (!this.#id) return;
    const mapped = serverRequestToPrompt(req, this.#id);
    if (!mapped) return;
    // The channel replays the same request object while it remains pending. Weak
    // keys let drafts expire on resolution, eviction, or a replacement channel.
    let lifetime = this.#promptLifetimes.get(req);
    if (!lifetime) {
      lifetime = { viewId: String(++this.#nextPromptView) };
      this.#promptLifetimes.set(req, lifetime);
    }
    const id = lifetime.viewId;
    if (this.#visiblePrompts.has(id)) return;
    this.#visiblePrompts.set(id, lifetime);
    if (mapped.kind === "approval") {
      this.prompts = {
        ...this.prompts,
        approvals: [...this.prompts.approvals, { ...mapped.approval, id }],
      };
    } else {
      this.prompts = {
        ...this.prompts,
        questions: [...this.prompts.questions, { ...mapped.call, id }],
      };
    }
  }

  #onNotification(notification: ServerNotification): void {
    if (!this.#id) return;
    if (notification.method === "turn/started" && notification.params.threadId === this.#id) {
      this.#clearPrediction();
    }
    if (notification.method === "thread/reverted" && notification.params.threadId === this.#id) {
      this.#clearPrediction();
    }
    if (notification.method === "turn/completed" && notification.params.threadId === this.#id) {
      if (notification.params.turn.status === "completed")
        this.#requestPrediction(notification.params.turn.id);
    }
    if (notification.method === "serverRequest/resolved") {
      const { requestId, threadId } = notification.params;
      if (threadId !== this.#id) return;
      const prompt =
        this.prompts.approvals.find((a) => a.requestId === requestId) ??
        this.prompts.questions.find((q) => q.requestId === requestId);
      if (prompt) this.#removePrompt(prompt.id);
      return;
    }
    if (
      notification.method === "thread/closed" ||
      notification.method === "thread/archived" ||
      notification.method === "thread/deleted"
    ) {
      if (notification.params.threadId === this.#id) this.#clearPrompts();
      return;
    }
    if (notification.method === "thread/queue/changed") {
      if (notification.params.threadId !== this.#id) return;
      void this.#refreshQueue();
    }
  }

  setPredictions(enabled: boolean): void {
    if (this.predictionsEnabled === enabled) return;
    this.predictionsEnabled = enabled;
    if (enabled) this.#requestPrediction(streams.current.latestCompletedTurnId);
    else this.#clearPrediction();
  }

  #clearPrediction(): void {
    this.#predictionRequest = undefined;
    this.prediction = undefined;
  }

  #requestPrediction(turnId: string | undefined): void {
    const machine = this.#machine;
    const threadId = this.#id;
    if (
      !this.predictionsEnabled ||
      !machine ||
      !threadId ||
      threadId !== this.#main ||
      !turnId ||
      this.#predictionRequest?.sourceTurnId === turnId
    )
      return;
    const request = { threadId, sourceTurnId: turnId };
    this.#predictionRequest = request;
    this.prediction = undefined;
    void machine
      .request<ThreadPredictionRequestResponse>("thread/prediction/request", request)
      .then(({ text }) => {
        if (request === this.#predictionRequest) this.prediction = text?.trim() || undefined;
      })
      .catch(() => {
        if (request === this.#predictionRequest) this.#clearPrediction();
      });
  }

  async #refreshQueue(): Promise<void> {
    if (!this.#id || !this.#machine || this.#machine.connection !== "open") return;
    const threadId = this.#id;
    const machine = this.#machine;
    const revision = ++this.#queueRead;
    try {
      // The native queue and list both have a maximum of 100 submissions.
      const response = await machine.request<ThreadQueueListResponse>("thread/queue/list", {
        threadId,
        limit: 100,
      });
      if (revision === this.#queueRead) this.queued = response.data ?? [];
    } catch {
      // Queue listing is optional.
    }
  }

  async sendInput(
    text: string,
    attachments: Attachment[] | DraftAttachments = [],
    options: {
      queue?: boolean;
      signal?: AbortSignal;
      skills?: SelectedSkill[];
      slashActions?: SlashActions;
      onRecovery?: () => void;
      onStarted?: (turnId: string) => void;
    } = {},
  ): Promise<string | undefined> {
    const id = this.#id;
    const machine = this.#machine;
    if (!id || !machine) return "Open a thread on an execution machine first.";
    const stream = streams.current;
    const draftAttachments: DraftAttachments = Array.isArray(attachments)
      ? { count: attachments.length, read: () => Promise.resolve(attachments) }
      : attachments;
    let shell: string | undefined;
    let input: UserInput[] = [];
    if (!options.onStarted && text.startsWith("!")) {
      shell = text.slice(1).trim();
      if (!shell) return "Enter a shell command after !.";
      if (draftAttachments.count) return "Shell commands do not accept attachments.";
    } else {
      if (!options.onStarted) {
        const slash = await this.#trySlashCommand(
          text,
          { id, machine },
          draftAttachments,
          !!options.queue,
          options.skills,
          options.signal,
          options.slashActions,
        );
        if (slash.handled) return slash.error;
      }
      input = toNativeUserInput(text, await draftAttachments.read(), options.skills);
      if (!input.length) return "Write a message or attach something.";
    }
    const draft: RecoveryDraft = {
      id: newClientMessageId(),
      input,
      shell,
      queued: !!options.queue,
      editing: false,
    };
    const activeTurn = stream.activeTurnId;
    this.pending++;
    this.error = undefined;
    try {
      const turn = await this.#submitDraft(machine, id, draft, activeTurn, options.signal);
      if (turn) options.onStarted?.(turn.id);
      return undefined;
    } catch (err) {
      const message = errorText(err, true);
      if (!(err instanceof TransportError && err.kind === "canceled")) {
        this.#updateRecoveries(machine, id, (list) => [...list, { ...draft, error: message }]);
        options.onRecovery?.();
      }
      return message;
    } finally {
      this.pending = Math.max(0, this.pending - 1);
    }
  }

  async #submitDraft(
    machine: Machine,
    threadId: string,
    draft: RecoveryDraft,
    activeTurn?: string,
    signal?: AbortSignal,
  ) {
    if (draft.shell !== undefined) {
      await machine.request(
        draft.queued ? "thread/queue/shellCommand" : "thread/shellCommand",
        { threadId, command: draft.shell },
        { signal },
      );
    } else {
      const common = {
        threadId,
        input: draft.input,
        clientUserMessageId: draft.clientUserMessageId ?? draft.id,
        ...(draft.responsesapiClientMetadata
          ? { responsesapiClientMetadata: draft.responsesapiClientMetadata }
          : {}),
      };
      if (draft.queued) {
        const params: ThreadQueueAddParams = common;
        await machine.request("thread/queue/add", params, { signal });
      } else if (activeTurn) {
        const params: TurnSteerParams = { ...common, expectedTurnId: activeTurn };
        await machine.request("turn/steer", params, { signal });
      } else {
        const params: TurnStartParams = common;
        const { turn } = await machine.request<TurnStartResponse>("turn/start", params, { signal });
        return turn;
      }
    }
    if (draft.queued && this.#machine === machine && this.#id === threadId)
      void this.#refreshQueue();
  }

  #updateRecoveries(
    machine: Machine,
    id: string,
    change: (list: RecoveryDraft[]) => RecoveryDraft[],
  ) {
    let threads = this.#recoveryByMachine.get(machine);
    if (!threads) {
      threads = new Map();
      this.#recoveryByMachine.set(machine, threads);
    }
    const entries = change(threads.get(id) ?? []);
    if (entries.length) threads.set(id, entries);
    else threads.delete(id);
    if (this.#machine === machine && this.#id === id) this.recoveries = entries;
  }

  async editWaiting(): Promise<void> {
    const machine = this.#machine;
    const threadId = this.#id;
    const queuedSubmissionIds = this.queued
      .filter((item) => item.kind === "message")
      .map(({ id }) => id);
    if (!machine || !threadId || !queuedSubmissionIds.length || this.pending) return;
    this.error = undefined;
    this.pending++;
    try {
      const { data } = await machine.request<{ data: QueuedSubmission[] }>("thread/queue/take", {
        threadId,
        queuedSubmissionIds,
      });
      this.#updateRecoveries(machine, threadId, (list) => [
        ...list,
        ...data.map((item): RecoveryDraft => ({
          id: item.id,
          clientUserMessageId: item.clientUserMessageId,
          input: item.input,
          queued: true,
          editing: true,
          responsesapiClientMetadata: item.responsesapiClientMetadata,
        })),
      ]);
    } catch (err) {
      if (this.#id === threadId && this.#machine === machine) this.error = errorText(err, true);
    } finally {
      this.pending--;
      if (this.#id === threadId && this.#machine === machine) void this.#refreshQueue();
    }
  }

  async steerWaiting(expectedTurnId: string): Promise<void> {
    const machine = this.#machine;
    const threadId = this.#id;
    if (!machine || !threadId || this.pending) return;
    const queued = this.queued.filter((item) => item.kind === "message");
    this.error = undefined;
    this.pending++;
    try {
      for (const item of queued) {
        const params: TurnSteerParams = {
          threadId,
          expectedTurnId,
          input: item.input,
          clientUserMessageId: item.clientUserMessageId,
          responsesapiClientMetadata: item.responsesapiClientMetadata,
        };
        await machine.request("turn/steer", params);
        await machine.request("thread/queue/delete", {
          threadId,
          queuedSubmissionId: item.id,
        });
      }
    } catch (err) {
      if (this.#id === threadId && this.#machine === machine) this.error = errorText(err, true);
    } finally {
      this.pending--;
      if (this.#id === threadId && this.#machine === machine) void this.#refreshQueue();
    }
  }

  async recover(id: string, action: "edit" | "discard" | "retry", edits?: string[]): Promise<void> {
    const machine = this.#machine;
    const threadId = this.#id;
    const draft = this.recoveries.find((entry) => entry.id === id);
    if (!machine || !threadId || !draft || this.pending) return;
    const replace = (next?: RecoveryDraft) =>
      this.#updateRecoveries(machine, threadId, (list) =>
        list.flatMap((entry) => (entry.id === id ? (next ? [next] : []) : [entry])),
      );
    if (action === "discard") return replace();
    if (action === "edit") return replace({ ...draft, editing: true });
    const edited = edits
      ? {
          ...draft,
          input: reviseRecoveryInput(draft.input, edits),
          shell:
            draft.shell === undefined
              ? undefined
              : (edits[0] ?? draft.shell).replace(/^!/, "").trim(),
        }
      : draft;
    if (
      edited.shell === "" ||
      (edited.shell === undefined &&
        !edited.input.some((part) => (part.type === "text" ? part.text.trim() : true)))
    ) {
      return replace({ ...edited, error: "Write a message or attach something." });
    }
    this.pending++;
    try {
      await this.#submitDraft(machine, threadId, edited, streams.current.activeTurnId);
      replace();
    } catch (err) {
      replace({ ...edited, error: errorText(err, true) });
    } finally {
      this.pending--;
    }
  }

  async #trySlashCommand(
    raw: string,
    { id, machine }: { id: string; machine: Machine },
    attachments: DraftAttachments,
    queue: boolean,
    skills?: SelectedSkill[],
    signal?: AbortSignal,
    actions?: SlashActions,
  ): Promise<{ handled: boolean; error?: string }> {
    const prefix = /^\s*(\/\S*)\s*/.exec(raw);
    if (!prefix) return { handled: false };
    const cmd = prefix[1].toLowerCase();
    const command = SLASH_COMMANDS.find(({ name }) => name === cmd);
    if (!command)
      return { handled: true, error: `Unknown command ${cmd}. Type / to see commands.` };

    const rest = raw.slice(prefix[0].length);
    const argument = "argument" in command ? command.argument : undefined;
    if (attachments.count && cmd !== "/btw" && cmd !== "/oneoff")
      return { handled: true, error: "This command does not accept attachments." };
    if (argument ? !rest : rest)
      return { handled: true, error: `Usage: ${cmd}${argument ? " " + argument : ""}` };

    switch (command.name) {
      case "/btw": {
        const main = this.#main ?? id;
        const question = rest.trimEnd();
        const input = toNativeUserInput(question, await attachments.read(), skills);
        let answers = this.#answersByMachine.get(machine);
        if (!answers) {
          answers = new Map();
          this.#answersByMachine.set(machine, answers);
        }
        let answer = answers.get(main);
        if (!answer) {
          answer = new SideAnswer(machine, main);
          answers.set(main, answer);
        }
        if (this.#machine === machine && this.#main === main) this.sideAnswer = answer;
        return { handled: true, error: await answer.ask(question, input) };
      }
      case "/compact":
        try {
          await machine.request(
            "thread/compact/start",
            { threadId: id, mode: queue ? "queue" : "continueInterrupted" },
            { signal },
          );
          return { handled: true };
        } catch (err) {
          return { handled: true, error: errorText(err, true) };
        }
      case "/archive":
        if (!actions) return { handled: true, error: "Confirm archive first." };
        actions.archive();
        return { handled: true };
      case "/rewind":
        return { handled: true, error: actions ? actions.rewind() : "Choose a message first." };
      case "/oneoff":
        return {
          handled: true,
          error: actions
            ? await actions.oneOff(
                rest,
                attachments.read,
                skills?.map((skill) => ({ ...skill, start: skill.start - prefix[0].length })),
              )
            : "Open One-off from the composer.",
        };
      case "/ps":
        streams.current.notice = "Active processes appear above the composer.";
        return { handled: true };
    }
  }

  async archive(): Promise<string | undefined> {
    const id = this.#id;
    const machine = this.#machine;
    if (!id || !machine) return "Open a thread on an execution machine first.";
    try {
      await machine.request("thread/archive", { threadId: id });
      return undefined;
    } catch (err) {
      return errorText(err, true);
    }
  }

  async interrupt(): Promise<string | undefined> {
    const id = this.#id;
    const machine = this.#machine;
    const turnId = streams.current.activeTurnId;
    if (!id || !machine || !turnId) return "No active turn to interrupt.";
    this.pending++;
    try {
      const params: TurnInterruptParams = { threadId: id, turnId };
      await machine.request("turn/interrupt", params);
      return undefined;
    } catch (err) {
      return errorText(err, true);
    } finally {
      this.pending = Math.max(0, this.pending - 1);
    }
  }

  async rename(name: string): Promise<string | undefined> {
    const id = this.#id;
    const machine = this.#machine;
    if (!id || !machine) return "Open a thread on an execution machine first.";
    const stream = streams.current;
    this.pending++;
    try {
      const params: ThreadSetNameParams = { threadId: id, name };
      await machine.request("thread/name/set", params);
      if (stream.session?.thread === id) {
        stream.session = { ...stream.session, title: name };
      }
      return undefined;
    } catch (err) {
      return errorText(err, true);
    } finally {
      this.pending = Math.max(0, this.pending - 1);
    }
  }

  async updateSettings(
    patch: Omit<ThreadSettingsUpdateParams, "threadId">,
  ): Promise<string | undefined> {
    const id = this.#id;
    const machine = this.#machine;
    if (!id || !machine) return "Open a thread on an execution machine first.";
    this.pending++;
    try {
      const params: ThreadSettingsUpdateParams = { threadId: id, ...patch };
      await machine.request("thread/settings/update", params);
      return undefined;
    } catch (err) {
      return errorText(err, true);
    } finally {
      this.pending = Math.max(0, this.pending - 1);
    }
  }

  async fork(lastTurnId?: string): Promise<{ id: string } | { error: string }> {
    const id = this.#id;
    const machine = this.#machine;
    if (!id || !machine) return { error: "Open a thread on an execution machine first." };
    this.pending++;
    try {
      const response = await machine.request<ThreadForkResponse>("thread/fork", {
        threadId: id,
        lastTurnId: lastTurnId ?? null,
      });
      return { id: response.thread.id };
    } catch (err) {
      return { error: errorText(err, true) };
    } finally {
      this.pending = Math.max(0, this.pending - 1);
    }
  }

  async revert(
    machine: Machine,
    threadId: string,
    beforeTurnId: string,
    beforeItemId: string | null,
    changeIds: string[] | null,
  ): Promise<string | undefined> {
    if (machine !== this.#machine || threadId !== this.#id || streams.current.activeTurnId)
      return "The selected thread is no longer idle and open.";
    this.pending++;
    try {
      await machine.request<ThreadRevertResponse>("thread/revert", {
        threadId,
        beforeTurnId,
        beforeItemId,
        ...(changeIds ? { expectedChangeIds: changeIds } : {}),
      });
      return undefined;
    } catch (err) {
      return errorText(err, true);
    } finally {
      this.pending = Math.max(0, this.pending - 1);
    }
  }

  decideApproval(approval: Approval, choiceIndex: number, reason = ""): string | undefined {
    const machine = this.#machine;
    if (!machine || machine.connection !== "open") {
      return "Wait for this machine to reconnect before answering.";
    }
    if (!this.prompts.approvals.includes(approval)) return "This approval is no longer available.";
    const choice = approval.choices.find((c) => c.id === choiceIndex);
    if (!choice) return "This approval choice is no longer available.";
    if (approval.method === "item/commandExecution/requestApproval") {
      const response: CommandExecutionRequestApprovalResponse = { decision: choice.decision };
      machine.respond(approval.requestId, response);
    } else if (approval.method === "item/fileChange/requestApproval") {
      const response: FileChangeRequestApprovalResponse = { decision: choice.decision };
      machine.respond(approval.requestId, response);
    } else if (approval.method === "shellGuard/requestApproval") {
      const response: ShellGuardRequestApprovalResponse = {
        decision: choice.decision,
        reason: choice.decision === "decline" && reason.trim() ? reason.trim() : null,
      };
      machine.respond(approval.requestId, response);
    }
    this.#removePrompt(approval.id);
    return undefined;
  }

  async answerQuestions(
    call: QuestionCall,
    answers: QuestionAnswer[],
    signal: AbortSignal,
  ): Promise<string | undefined> {
    const machine = this.#machine;
    if (!machine || machine.connection !== "open") {
      return "Wait for this machine to reconnect before answering.";
    }
    if (call.secretBlocked || call.questions.some((q) => q.isSecret)) {
      return "Secret prompts can only be answered locally on the execution machine.";
    }
    const payload: ToolRequestUserInputResponse = { answers: [] };
    for (const answer of answers) {
      const entry: ToolRequestUserInputAnswer = {
        id: answer.question,
        ...(answer.choice !== undefined ? { choice: answer.choice } : {}),
        ...(answer.note ? { note: answer.note } : {}),
        ...(answer.text ? { other: answer.text } : {}),
        ...(answer.skip ? { skipped: true } : {}),
      };
      if (!answer.items?.length) {
        payload.answers.push(entry);
        continue;
      }
      const uploads = await prepareAttachments(answer.items, signal, () => {});
      const images = uploads.flatMap((item) =>
        item.source === "upload"
          ? [`data:${item.mime || inferImageMime(item.name)};base64,${item.data}`]
          : [],
      );
      payload.answers.push({ ...entry, other: answer.text ?? "", images });
    }
    signal.throwIfAborted();
    if (machine !== this.#machine || call.threadId !== this.#id || machine.connection !== "open")
      return "Wait for this machine to reconnect before answering.";
    if (answers.some((answer) => answer.items?.length) && !this.prompts.questions.includes(call))
      return "This question is no longer available.";
    machine.respond(call.requestId, payload);
    this.#removePrompt(call.id);
    return undefined;
  }
}

export const semantic = new ThreadControl();
