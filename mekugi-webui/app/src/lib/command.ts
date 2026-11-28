import type { ThreadItem, Value } from "./protocol";

type CommandItem = Extract<ThreadItem, { type: "commandExecution" }>;

// Inline previews retain the last five nonblank lines, up to 512 UTF-8 bytes
// each. Details reads the complete aggregate, including lines clipped here.
const encoder = new TextEncoder();
const decoder = new TextDecoder();
function commandTail(output: string): Pick<Value, "tail" | "tailOmitted" | "tailClipped"> {
  const tail: string[] = [];
  let tailClipped = false;
  let end = output.length;
  while (end > 0 && tail.length < 5) {
    const start = output.lastIndexOf("\n", end - 1) + 1;
    const line = output.slice(start, end).replace(/\r$/, "").split("\r").at(-1)!.trimEnd();
    end = start - 1;
    if (!line.trim()) continue;
    const bytes = encoder.encode(line.slice(0, 513));
    if (bytes.length > 512) {
      tailClipped = true;
      let cut = 512;
      while ((bytes[cut] & 0xc0) === 0x80) cut--;
      tail.unshift(decoder.decode(bytes.subarray(0, cut)) + "…");
    } else {
      tail.unshift(line);
    }
  }
  let tailOmitted = 0;
  for (let i = 0; i <= end; i++) if (output[i] === "\n") tailOmitted++;
  return { tail: tail.length ? tail : undefined, tailOmitted, tailClipped };
}

const commandVerbs: Readonly<Record<string, string>> = {
  read: "Read",
  search: "Search",
  list: "List",
  listFiles: "List",
  inspect: "Inspect",
  stage: "Stage",
  commit: "Commit",
  skill: "Skill",
};

function commandVerb(item: CommandItem, kind?: string): string {
  if (kind && commandVerbs[kind]) return commandVerbs[kind];
  if (item.status === "declined") return "Denied";
  return item.status === "inProgress" ? "Running" : "Ran";
}

function commandSegments(item: CommandItem): string[] {
  return item.display?.segments ?? (item.commandActions ?? []).map((action) => action.command);
}

export function commandResult(item: CommandItem): Value {
  return {
    kind: "op",
    verb: commandVerb(item),
    workdir: item.display?.effectiveCwd || item.cwd || undefined,
    batchExit: commandSegments(item).length > 1,
    running: item.status === "inProgress",
    failed: item.status === "failed" || item.status === "declined",
    exitCode: item.exitCode ?? undefined,
    duration: item.durationMs ?? undefined,
  };
}

export function commandValues(item: CommandItem): Value[] {
  const segments = commandSegments(item);
  const kinds = item.display?.stepKinds;
  const subjects = item.display?.stepSubjects;
  const actions = item.commandActions ?? [];
  if (segments.length < 2)
    return [
      {
        ...commandResult(item),
        verb: commandVerb(item, kinds?.[0] ?? (actions.length === 1 ? actions[0].type : undefined)),
        code: subjects?.[0] ?? item.display?.segments[0] ?? item.display?.script ?? item.command,
        lang: subjects?.[0] ? "text" : "bash",
        ...commandTail(item.aggregatedOutput ?? ""),
      },
    ];
  const workdir = item.display?.effectiveCwd || item.cwd || undefined;
  // Source steps describe the script, not which steps ran. Details owns the result.
  return segments.map((code, i) => ({
    kind: "op",
    verb: commandVerb(item, kinds?.[i] ?? (!item.display ? actions[i]?.type : undefined)),
    code: subjects?.[i] ?? code,
    lang: subjects?.[i] ? "text" : "bash",
    workdir,
  }));
}
