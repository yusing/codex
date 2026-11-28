// Native app-server protocol types reused from codex-rs/app-server-protocol/schema/typescript
// plus lightweight UI projection types for the WebUI components.

export type { ClientInfo } from "@codex-protocol/ClientInfo";
export type { ClientNotification } from "@codex-protocol/ClientNotification";
export type { InitializeCapabilities } from "@codex-protocol/InitializeCapabilities";
export type { InitializeParams } from "@codex-protocol/InitializeParams";
export type { InitializeResponse } from "@codex-protocol/InitializeResponse";
export type { ReasoningEffort } from "@codex-protocol/ReasoningEffort";
export type { RequestId } from "@codex-protocol/RequestId";
export type { ServerNotification } from "@codex-protocol/ServerNotification";

import type { ServerRequest as NativeServerRequest } from "@codex-protocol/ServerRequest";
import type { AdditionalPermissionProfile } from "@codex-protocol/v2/AdditionalPermissionProfile";
import type { CommandExecutionApprovalDecision } from "@codex-protocol/v2/CommandExecutionApprovalDecision";

export type { CommandExecutionApprovalDecision } from "@codex-protocol/v2/CommandExecutionApprovalDecision";
export type { CommandExecutionChangesUpdatedNotification } from "@codex-protocol/v2/CommandExecutionChangesUpdatedNotification";
export type { CommandExecutionRequestApprovalParams } from "@codex-protocol/v2/CommandExecutionRequestApprovalParams";
export type { CommandExecutionRequestApprovalResponse } from "@codex-protocol/v2/CommandExecutionRequestApprovalResponse";
export type { ComposerPreset } from "@codex-protocol/v2/ComposerPreset";
export type { FileChangeApprovalDecision } from "@codex-protocol/v2/FileChangeApprovalDecision";
export type { FileChangeRequestApprovalParams } from "@codex-protocol/v2/FileChangeRequestApprovalParams";
export type { FileChangeRequestApprovalResponse } from "@codex-protocol/v2/FileChangeRequestApprovalResponse";
export type { FileUpdateChange } from "@codex-protocol/v2/FileUpdateChange";
export type { HookRunSummary } from "@codex-protocol/v2/HookRunSummary";
export type { Model } from "@codex-protocol/v2/Model";
export type { ModelListParams } from "@codex-protocol/v2/ModelListParams";
export type { ModelListResponse } from "@codex-protocol/v2/ModelListResponse";
export type { PermissionProfileListResponse } from "@codex-protocol/v2/PermissionProfileListResponse";
export type { QueuedSubmission } from "@codex-protocol/v2/QueuedSubmission";
export type { ShellGuardApprovalDecision } from "@codex-protocol/v2/ShellGuardApprovalDecision";
export type { ShellGuardRequestApprovalParams } from "@codex-protocol/v2/ShellGuardRequestApprovalParams";
export type { ShellGuardRequestApprovalResponse } from "@codex-protocol/v2/ShellGuardRequestApprovalResponse";
export type { SkillsListParams } from "@codex-protocol/v2/SkillsListParams";
export type { SkillsListResponse } from "@codex-protocol/v2/SkillsListResponse";
export type { ThreadArchiveParams } from "@codex-protocol/v2/ThreadArchiveParams";
export type { ThreadChangeFileDiff } from "@codex-protocol/v2/ThreadChangeFileDiff";
export type { ThreadChangeOrigin } from "@codex-protocol/v2/ThreadChangeOrigin";
export type { ThreadChangesLiveResponse } from "@codex-protocol/v2/ThreadChangesLiveResponse";
export type { ThreadChangesReadParams } from "@codex-protocol/v2/ThreadChangesReadParams";
export type { ThreadChangesReadResponse } from "@codex-protocol/v2/ThreadChangesReadResponse";
export type { ThreadFileSearchResponse } from "@codex-protocol/v2/ThreadFileSearchResponse";
export type { ThreadForkParams } from "@codex-protocol/v2/ThreadForkParams";
export type { ThreadForkResponse } from "@codex-protocol/v2/ThreadForkResponse";
export type { ThreadItem } from "@codex-protocol/v2/ThreadItem";
export type { ThreadListParams } from "@codex-protocol/v2/ThreadListParams";
export type { ThreadListResponse } from "@codex-protocol/v2/ThreadListResponse";
export type { ThreadPresetsListResponse } from "@codex-protocol/v2/ThreadPresetsListResponse";
export type { ThreadPresetWriteParams } from "@codex-protocol/v2/ThreadPresetWriteParams";
export type { ThreadPresetWriteResponse } from "@codex-protocol/v2/ThreadPresetWriteResponse";
export type { ThreadReadParams } from "@codex-protocol/v2/ThreadReadParams";
export type { ThreadReadResponse } from "@codex-protocol/v2/ThreadReadResponse";
export type { ThreadResumeParams } from "@codex-protocol/v2/ThreadResumeParams";
export type { ThreadRevertParams } from "@codex-protocol/v2/ThreadRevertParams";
export type { ThreadRevertResponse } from "@codex-protocol/v2/ThreadRevertResponse";
export type { ThreadSetNameParams } from "@codex-protocol/v2/ThreadSetNameParams";
export type { ThreadSettings } from "@codex-protocol/v2/ThreadSettings";
export type { ThreadStartParams } from "@codex-protocol/v2/ThreadStartParams";
export type { ThreadStatus } from "@codex-protocol/v2/ThreadStatus";
export type { ThreadSubtreeListParams } from "@codex-protocol/v2/ThreadSubtreeListParams";
export type { ThreadTokenUsage } from "@codex-protocol/v2/ThreadTokenUsage";
export type { ToolRequestUserInputAnswer } from "@codex-protocol/v2/ToolRequestUserInputAnswer";
export type { ToolRequestUserInputOption } from "@codex-protocol/v2/ToolRequestUserInputOption";
export type { ToolRequestUserInputParams } from "@codex-protocol/v2/ToolRequestUserInputParams";
export type { ToolRequestUserInputQuestion } from "@codex-protocol/v2/ToolRequestUserInputQuestion";
export type { ToolRequestUserInputResponse } from "@codex-protocol/v2/ToolRequestUserInputResponse";
export type { Turn } from "@codex-protocol/v2/Turn";
export type { TurnInterruptParams } from "@codex-protocol/v2/TurnInterruptParams";
export type { TurnStartParams } from "@codex-protocol/v2/TurnStartParams";
export type { TurnStartResponse } from "@codex-protocol/v2/TurnStartResponse";
export type { TurnsPage } from "@codex-protocol/v2/TurnsPage";
export type { UserInput } from "@codex-protocol/v2/UserInput";

import type { ReasoningEffort } from "@codex-protocol/ReasoningEffort";
import type { RequestId } from "@codex-protocol/RequestId";
import type { ActivePermissionProfile } from "@codex-protocol/v2/ActivePermissionProfile";
import type { QueuedSubmission } from "@codex-protocol/v2/QueuedSubmission";
import type { Thread as StableThread } from "@codex-protocol/v2/Thread";
import type { ThreadExtra } from "@codex-protocol/v2/ThreadExtra";
import type { ThreadItem } from "@codex-protocol/v2/ThreadItem";
import type { ThreadResumeResponse as StableThreadResumeResponse } from "@codex-protocol/v2/ThreadResumeResponse";
import type { ThreadStartResponse as StableThreadStartResponse } from "@codex-protocol/v2/ThreadStartResponse";
import type { Turn } from "@codex-protocol/v2/Turn";
import type { TurnSteerParams as StableTurnSteerParams } from "@codex-protocol/v2/TurnSteerParams";
import type { TurnsPage } from "@codex-protocol/v2/TurnsPage";
import type { UserInput } from "@codex-protocol/v2/UserInput";

export const protocolVersion = "9" as const;
export const subprotocol = "mekugi.control.v9" as const;

export type Thread = StableThread & { extra?: ThreadExtra | null };
export type ThreadResumeResponse = StableThreadResumeResponse & {
  activePermissionProfile?: ActivePermissionProfile | null;
  initialTurnsPage?: TurnsPage | null;
};
export type ThreadStartResponse = StableThreadStartResponse & {
  activePermissionProfile?: ActivePermissionProfile | null;
};
export type TurnSteerParams = StableTurnSteerParams & {
  responsesapiClientMetadata?: QueuedSubmission["responsesapiClientMetadata"];
};

export type BackgroundTerminal = {
  threadId: string;
  turnId: string;
  itemId: string;
  processId: string;
  command: string;
};

export type BackgroundTerminalsPage = {
  data: Omit<BackgroundTerminal, "threadId">[];
  nextCursor: string | null;
};

// Methods admitted by codex-rs/app-server/src/message_processor.rs (is_allowed_for_remote_human).
export type RemoteHumanMethod =
  | "initialize"
  | "model/list"
  | "permissionProfile/list"
  | "skills/list"
  | "featureUse/read"
  | "server/diagnostics"
  | "review/start"
  | "turn/start"
  | "turn/steer"
  | "turn/interrupt"
  | "turn/settings/update"
  | "thread/start"
  | "thread/resume"
  | "thread/fork"
  | "thread/archive"
  | "thread/unarchive"
  | "thread/delete"
  | "thread/unsubscribe"
  | "thread/name/set"
  | "thread/presets/list"
  | "thread/presets/write"
  | "thread/queue/add"
  | "thread/queue/shellCommand"
  | "thread/queue/list"
  | "thread/queue/take"
  | "thread/queue/delete"
  | "thread/settings/update"
  | "thread/compact/start"
  | "thread/compact/cancel"
  | "thread/shellCommand"
  | "thread/backgroundTerminals/list"
  | "thread/backgroundTerminals/terminate"
  | "thread/revert"
  | "thread/revert/preview"
  | "thread/prediction/request"
  | "thread/list"
  | "thread/inspect"
  | "thread/subtree/list"
  | "thread/changes/live"
  | "thread/changes/read"
  | "thread/files/search"
  | "thread/read"
  | "thread/turns/list"
  | "thread/items/list"
  | "thread/items/read";

// Experimental native RPC payloads whose Rust definitions are gated behind experimentalApi.
type CommandApprovalRequest = Extract<
  NativeServerRequest,
  { method: "item/commandExecution/requestApproval" }
>;
export type ServerRequest =
  | Exclude<NativeServerRequest, CommandApprovalRequest>
  | (CommandApprovalRequest & {
      params: {
        additionalPermissions?: AdditionalPermissionProfile | null;
        availableDecisions?: CommandExecutionApprovalDecision[] | null;
      };
    });

export type ThreadPredictionRequestResponse = { text: string | null };

export type ThreadSettingsUpdateParams = {
  threadId: string;
  model?: string | null;
  effort?: ReasoningEffort | null;
  serviceTier?: string | null;
  permissions?: string;
};

export type ThreadQueueAddParams = {
  threadId: string;
  input: Array<UserInput>;
  clientUserMessageId: string;
};

export type ThreadQueueListParams = {
  threadId: string;
  cursor?: string | null;
  limit?: number | null;
};

export type ThreadQueueListResponse = {
  data: Array<QueuedSubmission>;
  nextCursor?: string | null;
};

export type ContentResourceRequest = {
  thread: string;
  body?: {
    thread?: string;
    turnId?: string;
    itemId?: string;
  };
};

export type ContentResourceResponse = {
  thread?: Thread;
  threadId?: string;
  turn?: Turn;
  turnId?: string;
  item?: ThreadItem;
};

export type ImageResourceRequest = {
  thread: string;
  body: {
    itemId?: string;
    index?: number;
    image: string;
  };
};

export type AttachmentImageResourceRequest = {
  thread?: string;
  body: {
    path: string;
    workspace?: string;
    probe?: boolean;
  };
};

export type ResourceRequest =
  | ContentResourceRequest
  | { thread: string; body: { path: string } }
  | ImageResourceRequest
  | AttachmentImageResourceRequest
  | { thread?: never; body?: never };

// --- UI projection types rendered by Svelte components ---

export type Status = {
  text?: string;
  failure?: string;
  alert?: boolean;
  running?: boolean;
  completed?: number | null;
};

export type Listed = {
  parentThread?: string;
  id?: string;
  thread?: string;
  title?: string;
  workspace?: string;
  started?: number;
  status: Status;
  waiting?: number;
};

export type Session = {
  thread: string;
  caller?: string;
  role?: string;
  title?: string;
  workspace?: string;
  started?: number;
  model?: string;
  effort?: string;
  serviceTier?: string;
  permissionProfile?: string | null;
};

export type Input = {
  reason?: string;
  stoppable?: boolean;
  waiting?: number;
};

export type AgentIdentity = {
  name: string;
  label: string;
  aliases: Array<string>;
};

export type TextSpan = {
  Start: number;
  End: number;
  Kind: number;
};

export type Traffic = {
  kind: string;
  from?: string;
  to?: string;
  otherRecipients?: string[];
  detail?: string;
  incoming?: boolean;
};

export type Read = {
  path: string;
  range?: string;
  ranges?: Array<string>;
  note?: string;
};

export type Change = {
  kind: string;
  verb?: string;
  label?: string;
  path?: string;
  from?: string;
  to?: string;
  code?: string;
  note?: string;
  footer?: string;
  added?: number;
  removed?: number;
  old?: Array<string>;
  new?: Array<string>;
  truncated?: boolean;
};

export type Hook = {
  event?: string;
  name?: string;
  message?: string;
  outcome?: string;
  status?: string;
  hasError?: boolean;
  blocked?: boolean;
  denied?: boolean;
};

export type QuestionSummary = {
  header?: string;
  question: string;
  text?: string;
  answer?: string;
  note?: string;
  skipped?: boolean;
};

export type WaitTarget = {
  name: string;
  status?: string;
  role?: string;
  summary?: string;
};

export type Disclosure = {
  owner?: string;
};

export type Value = {
  params?: Array<Value>;
  failed?: boolean;
  kind?: string;
  verb?: string;
  batchExit?: boolean;
  label?: string;
  sources?: Array<string>;
  path?: string;
  autoReload?: boolean;
  approval?: string;
  code?: string;
  opaqueJSON?: boolean;
  codePreview?: string;
  detailCode?: string;
  detailsAvailable?: Disclosure | null;
  lang?: string;
  fenced?: boolean;
  body?: string;
  details?: string;
  summary?: string;
  reads?: Array<Read>;
  subsumed?: boolean;
  effect?: boolean;
  pending?: boolean;
  skipped?: boolean;
  hiddenEdits?: number;
  codeLines?: number;
  tailLines?: number;
  foldSource?: boolean;
  duration?: number;
  thought?: number;
  workdir?: string;
  from?: string;
  to?: string;
  questions?: Array<QuestionSummary>;
  waits?: Array<WaitTarget>;
  waitReason?: string;
  waitTimeout?: string;
  exitCode?: number | null;
  editOutcome?: string;
  running?: boolean;
  live?: boolean;
  tail?: Array<string>;
  tailOmitted?: number;
  tailClipped?: boolean;
  sourceOmitted?: boolean;
  changes?: Array<Change>;
  hook?: Hook | null;
};

export type ContentReference = {
  turnId?: string;
  itemId?: string;
};

export type Entry = {
  rowKey?: string;
  itemId?: string;
  turnId?: string;
  traffic?: Traffic | null;
  identities?: Array<AgentIdentity>;
  position: number;
  turn?: number;
  revision?: number;
  content?: ContentReference | null;
  autoContent?: boolean;
  spans?: Array<TextSpan>;
  images?: Array<string>;
  audio?: Array<string>;
  seq: number;
  agent?: string;
  kind?: string;
  outcome?: string;
  text?: string;
  observed?: number;
  running?: boolean;
  assignment?: boolean;
  assignmentTo?: string;
  assignmentText?: string;
  values?: Array<Value>;
};

export type AgentState = "working" | "waiting" | "error" | "done" | "idle" | "paused";

export type Agent = {
  name: string;
  label: string;
  aliases?: Array<string>;
  identities?: Array<AgentIdentity>;
  thread?: string;
  nonAttachable?: boolean;
  role?: string;
  depth?: number;
  state: AgentState;
  wait?: Value | null;
  activity?: Entry | null;
  turns?: number;
  roundtrips?: number;
  roundtripsPartial?: boolean;
  work?: number | null;
  workSince?: number;
  lastResponse?: number;
  lastCompleted?: number;
  cost?: number | null;
  tps?: number | null;
  costPartial?: boolean;
  inputTokens?: number | null;
  cachedInputTokens?: number | null;
  outputTokens?: number | null;
  reasoningOutputTokens?: number | null;
  usagePartial?: boolean;
  context?: number | null;
  window?: number;
  added?: number | null;
  removed?: number | null;
  skills?: Array<string>;
  items?: Array<string>;
};

export type Roster = {
  total: number;
  agents: Array<Agent>;
  added?: number;
  removed?: number;
};

export type Counts = {
  Added: number;
  Removed: number;
};

export type Origin = {
  Change: string;
  Caller: string;
  Source: string;
};

export type ReviewFile = {
  Path?: string;
  BeforePath?: string;
  AfterPath?: string;
  CopyFrom?: string;
  Action?: string;
  Source?: string;
  Diff: string;
  Binary?: boolean;
  Directory?: boolean;
  Link?: boolean;
  Incomplete?: string;
  Added?: number;
  Removed?: number;
  Origin?: string;
  OriginNote?: string;
  Scope?: string;
};

export type Chunk = Origin & {
  Key: string;
  Status: string;
  Stream: string;
  Workspace: string;
  CaptureOrder: number;
  Review: ReviewFile;
  Highlighted: boolean;
};

export type ReviewedFile = {
  key: string;
  path: string;
  status?: string;
  chunks: Array<Chunk>;
  origins: Array<Origin>;
  counts: Counts;
};

export type CallerReview = {
  caller: string;
  count: number;
  counts: Counts;
};

export type SavedReview = {
  caller: string;
  fileCount: number;
  revision: number;
  workspace: string;
  coverage: string;
  failure?: string;
  callers: Array<CallerReview>;
  counts: {
    Callers: Record<string, Counts>;
    Net: Counts | null;
  };
  files: Array<ReviewedFile>;
};

export type Preview = {
  ID: string;
  Workspace: string;
  Caller: string;
  Turn?: string;
  Thread: string;
  Files: Array<ReviewFile>;
  Complete?: boolean;
  Settling?: boolean;
  SavedPath?: string;
  ExternalSummary?: string;
  Status: string;
  Footer?: string;
  Tool?: string;
  Withdrawn?: boolean;
};

export type ApprovalChoice = {
  id: number;
  label: string;
  decision: "accept" | "acceptForSession" | "decline";
  deny?: boolean;
};

export type Approval = {
  id: string;
  requestId: RequestId;
  method:
    | "item/commandExecution/requestApproval"
    | "item/fileChange/requestApproval"
    | "shellGuard/requestApproval";
  threadId: string;
  agent?: string;
  title: string;
  subject?: string;
  details: Array<string>;
  choices: Array<ApprovalChoice>;
};

export type QuestionOption = {
  label: string;
  description?: string;
  preview?: string;
  recommended?: boolean;
};

export type Question = {
  id: string;
  header?: string;
  question: string;
  text: string;
  isOther?: boolean;
  isSecret?: boolean;
  secret?: boolean;
  options?: Array<QuestionOption>;
};

export type QuestionCall = {
  id: string;
  requestId: RequestId;
  threadId: string;
  blocking: boolean;
  secretBlocked?: boolean;
  agent?: string;
  questions: Array<Question>;
};

export type Prompts = {
  questions: Array<QuestionCall>;
  approvals: Array<Approval>;
};

export type StartModel = {
  id: string;
  label: string;
  efforts: Array<string>;
  defaultEffort?: string;
  serviceTiers?: Array<string>;
  defaultServiceTier?: string;
};

export type ModelGroup = {
  label: string;
  models: Array<StartModel>;
};

export type StartOptions = {
  models: Array<ModelGroup>;
  defaultModel: string;
  note?: string;
};

export type Start = {
  workspace?: string;
  model?: string;
  effort?: string;
};

export type Attachment = { placeholder?: string } & (
  | { source: "path"; path: string; fileReference?: boolean }
  | { source: "upload"; name: string; data: string; mime?: string }
  | { source: "textUpload"; name: string; data: string }
  | { source: "selection"; name: string; data: string; path?: string; inline?: boolean }
);
