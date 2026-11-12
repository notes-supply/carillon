// @ts-check
/**
 * What the page decides, apart from the document: what a poll compares, how
 * turns are merged, how a message waits for its receipt, and what a turn
 * shows for each state. Nothing here reads the document or makes a request.
 */

/** @typedef {{ id: string, agent_id: string, title: string, started_at: number, active_at: number, activity: number, at_work: number }} Conversation */
/** @typedef {{ name: string, size: number, type: string, url: string }} FileLink */
/** @typedef {{ kind: "person", event_id: string, text: string, command: boolean, files: FileLink[] } | { kind: "runtime", label: string | null }} From */
/** @typedef {"steer" | "interrupt" | "stop"} Control */
/** @typedef {"queued" | "running" | "waiting" | "succeeded" | "failed" | "cancelled" | "unknown"} WorkState */
/** @typedef {{ id: string, kind: "task" | "delegation" | "handoff" | "fork", label: string, state: WorkState, actor?: string, started_at?: number, duration_ms?: number }} Work */
/**
 * @typedef {object} Activity The runtime's snapshot of a turn, as the provider passes it on.
 * @property {"starting" | "queued" | "model" | "tools" | "waiting" | "retrying" | "compacting"} phase
 * @property {string} [headline]
 * @property {{ tool: string, detail?: string, started_at: number, estimated?: boolean }[]} running
 * @property {{ tool: string, detail?: string, ok: boolean, duration_ms?: number }[]} recent
 * @property {number} tool_calls
 * @property {{ items: Work[], partial: boolean, omitted?: number }} [work]
 * @property {string[]} [notes]
 * @property {{ model?: string, effort?: string, context?: { used: number, window?: number } }} [agent]
 * @property {boolean} [partial]
 * @property {Control[]} [controls]
 */
/** @typedef {{ id: string, from: From, accepted_at: number, finished_at: number | null, report: Activity | null } & ({ state: "queued" | "running" | "steered" | "stopped" } | { state: "answered", html: string, files: FileLink[] } | { state: "interrupted", html: string })} Turn */
/** @typedef {{ id: string, state: "queued" | "running" | "ended", activity: Activity | null }} Live */
/** @typedef {{ turns: Turn[], before: string | null }} Page A conversation's turns, oldest first, and where older ones continue. */
/** @typedef {{ name: string, description?: string, hint?: string }} Command */
/** @typedef {{ files: number, file_bytes: number, message_bytes: number }} Limits */
/**
 * @typedef {object} Pending A message until its turn is on screen.
 * @property {string} eventId Minted once, and sent again with every attempt.
 * @property {string | null} agent The agent, for a conversation's first message.
 * @property {boolean} command
 * @property {string} text
 * @property {File[]} files
 * @property {"uploading" | "receiving" | "retrying" | "received"} stage
 * @property {number} sent Bytes uploaded in this attempt.
 * @property {number} total
 */
/** @typedef {"past" | "done" | "failed" | "running" | "waiting"} Mark One mark of the row: a call no longer listed, finished, failed, under way or not started. */
/** @typedef {{ mark: Mark, name: string, detail: string, time: string }} Line */
/** @typedef {{ action: Control, label: string, title: string, asked: boolean }} Offer */
/** @typedef {{ phase: string, waiting: boolean, tally: string, headline: string, marks: Mark[], calls: Line[], work: Line[], notes: string[], controls: Offer[] }} Ledger */

/**
 * Whether the poll says a conversation's turns must be read again: its
 * `activity` or `at_work` is not what the last read saw. A conversation the
 * first page does not list has not changed.
 * @param {Conversation | undefined} seen
 * @param {Conversation | undefined} listed
 */
export function changed(seen, listed) {
  if (listed === undefined) return false;
  return seen?.activity !== listed.activity || seen.at_work !== listed.at_work;
}

/**
 * Whether a conversation has something the person has not looked at: its
 * activity is past what it was when they last had it open, or when the page
 * first listed it.
 * @param {number | undefined} looked
 * @param {Conversation} listed
 */
export function unread(looked, listed) {
  return looked !== undefined && listed.activity > looked;
}

/**
 * The newest page of turns laid over the turns held: each turn it has
 * replaces the held one of the same ID. A page that shares no turn with
 * those held leaves a gap, so it replaces them all.
 * @param {Page} held
 * @param {Page} page
 * @returns {Page}
 */
export function merge(held, page) {
  const fresh = new Set(page.turns.map((turn) => turn.id));
  const whole = page.before === null;
  const gap = !held.turns.some((turn) => fresh.has(turn.id));
  if (whole || gap) return page;
  return {
    turns: [...held.turns.filter((turn) => !fresh.has(turn.id)), ...page.turns],
    before: held.before,
  };
}

/**
 * An older page put before the turns held.
 * @param {Page} held
 * @param {Page} older
 * @returns {Page}
 */
export function earlier(held, older) {
  const known = new Set(held.turns.map((turn) => turn.id));
  return {
    turns: [...older.turns.filter((turn) => !known.has(turn.id)), ...held.turns],
    before: older.before,
  };
}

/**
 * Whether turns read at `readAt` may hold file links that no longer work. A
 * link is the same for a whole UTC day and works until the end of the next,
 * so turns read on an earlier UTC day are read again.
 * @param {number} readAt Unix seconds.
 * @param {number} now Unix seconds.
 */
export function linksAged(readAt, now) {
  return Math.floor(readAt / 86_400) !== Math.floor(now / 86_400);
}

/**
 * The turns a live read asks about: the newest unfinished ones, as many as
 * one read takes.
 * @param {Turn[]} turns
 */
export function unfinished(turns) {
  return turns
    .filter((turn) => turn.state === "queued" || turn.state === "running")
    .slice(-LIVE_READ_LIMIT)
    .map((turn) => turn.id);
}

/**
 * Numbers requests of one kind, so an answer can tell whether a newer
 * request, or a move to another conversation, has superseded its own.
 */
export function sequence() {
  let latest = 0;
  return {
    next: () => ++latest,
    /** @param {number} ticket */
    current: (ticket) => ticket === latest,
  };
}

/**
 * What an answer's status means for the page. Status 0 is a request that
 * got no answer: the provider is not running.
 * @param {number} status
 * @returns {"ok" | "away" | "signed-out" | "refused"}
 */
export function reading(status) {
  if (status >= 200 && status < 300) return "ok";
  if (status === 0 || status === 503) return "away";
  return status === 401 ? "signed-out" : "refused";
}

export const AWAY = "Carillon is not answering. Trying again.";
export const SIGNED_OUT =
  "This browser is signed out. Run `carillon web open` in a terminal to sign it in.";

/**
 * The words for an answer that is not 2xx: the provider's own when it gave
 * any, or a sentence for the status. A stopped provider and a core that is
 * away are one thing to the person, so both get the page's sentence.
 * @param {number} status
 * @param {string} body
 */
export function refusal(status, body) {
  if (reading(status) === "away") return AWAY;
  try {
    /** @type {unknown} */
    const parsed = JSON.parse(body);
    if (typeof parsed === "object" && parsed !== null && "error" in parsed) {
      const { error } = parsed;
      if (typeof error === "string" && error !== "")
        return error.charAt(0).toUpperCase() + error.slice(1);
    }
  } catch {
    // Not JSON: the status decides.
  }
  return status === 401
    ? SIGNED_OUT
    : `Carillon refused the request with status ${String(status)}.`;
}

/**
 * How long to wait before the next poll or attempt, in milliseconds: about a
 * second, and longer after each failure in a row, up to ten.
 * @param {number} failures
 */
export function wait(failures) {
  return Math.min(1000 * 2 ** failures, 10_000);
}

/**
 * Why core would refuse these files, or undefined when it takes them.
 * @param {readonly { name: string, size: number }[]} files
 * @param {Limits} limits
 */
export function overLimit(files, limits) {
  if (files.length > limits.files)
    return `A message takes at most ${String(limits.files)} files. Remove ${String(files.length - limits.files)}.`;
  const large = files.find((file) => file.size > limits.file_bytes);
  if (large !== undefined)
    return `${large.name} is ${size(large.size)}. A file can be at most ${size(limits.file_bytes)}.`;
  const total = files.reduce((sum, file) => sum + file.size, 0);
  if (total > limits.message_bytes)
    return `These files are ${size(total)} together. A message takes at most ${size(limits.message_bytes)}.`;
  return undefined;
}

/**
 * Two texts as one message, with a blank line between them. A stored draft
 * is this until the receipt: the message on its way, then what was written
 * since.
 * @param {string} first
 * @param {string} second
 */
export function joined(first, second) {
  return first !== "" && second !== "" ? `${first}\n\n${second}` : first + second;
}

/**
 * What was written since a message was sent: the stored draft without that
 * message. On the receipt, the draft is cleared of what was sent and of
 * nothing else.
 * @param {string} stored
 * @param {string} sent
 */
export function since(stored, sent) {
  if (stored === sent) return "";
  return sent !== "" && stored.startsWith(`${sent}\n\n`) ? stored.slice(sent.length + 2) : stored;
}

/**
 * A message's text with the line ends a textarea has. A browser's form
 * sends CRLF, and core keeps them.
 * @param {string} text
 */
export function plain(text) {
  return text.replaceAll("\r\n", "\n");
}

/**
 * The messages still to draw as pending: those whose turn is not among the
 * turns on screen.
 * @param {Pending[]} pending
 * @param {Turn[]} turns
 */
export function awaiting(pending, turns) {
  const arrived = new Set(turns.map(({ from }) => (from.kind === "person" ? from.event_id : "")));
  return pending.filter((message) => !arrived.has(message.eventId));
}

/**
 * What a pending message says about itself. After the last byte the request
 * stays open while core copies the files or takes an earlier message, and
 * the page cannot tell which.
 * @param {Pending} pending
 */
export function sendingLine({ stage, sent, total, files }) {
  const share =
    files.length > 0 && total > 0 ? `, ${String(Math.floor((sent / total) * 100))}%` : "";
  return {
    uploading: `Sending${share}`,
    receiving: "Carillon is receiving it.",
    retrying: "Carillon is not answering. This message will be sent when it does.",
    received: "Received.",
  }[stage];
}

/**
 * The commands a text that is still only `/` and a prefix could become.
 * @param {string} text
 * @param {Command[]} commands
 */
export function offered(text, commands) {
  if (!text.startsWith("/")) return [];
  return commands.filter((command) => command.name.startsWith(text.slice(1)));
}

/**
 * Whether a text invokes one of the runtime's commands: `/`, the command's
 * name, and then nothing or its arguments.
 * @param {string} text
 * @param {Command[]} commands
 */
export function invokes(text, commands) {
  const [first = ""] = text.split(/\s/u, 1);
  return commands.some((command) => first === `/${command.name}`);
}

const PHASES = {
  starting: "Starting",
  queued: "Waiting its turn",
  model: "Waiting for the model",
  tools: "Using tools",
  waiting: "Waiting",
  retrying: "Retrying the model request",
  compacting: "Compacting the conversation",
};

/**
 * @type {Record<Control, { label: string, pressed: string, waiting: string, answering: string }>}
 */
const CONTROLS = {
  steer: {
    label: "Steer",
    pressed: "Steering",
    waiting: "Hand this message to the answer that is running.",
    answering: "Hand this message to the answer that is running.",
  },
  interrupt: {
    label: "Interrupt",
    pressed: "Interrupting",
    waiting: "End the running answer and run this message next.",
    answering: "End the running answer and run this message next.",
  },
  stop: {
    label: "Stop",
    pressed: "Stopping",
    waiting: "Do not run this message.",
    answering: "End this answer.",
  },
};

const WORK = { task: "Task", delegation: "Delegated", handoff: "Handed off", fork: "Fork" };

/** @type {Record<WorkState, [Mark, string]>} */
const WORK_STATES = {
  queued: ["waiting", "queued"],
  running: ["running", "running"],
  waiting: ["waiting", "waiting"],
  succeeded: ["done", "done"],
  failed: ["failed", "failed"],
  cancelled: ["past", "cancelled"],
  unknown: ["past", "state unknown"],
};

const MAX_PAST_MARKS = 48;
/** How many turns the provider takes in one live read. */
const LIVE_READ_LIMIT = 32;
const PRESSED_FOR_SECONDS = 15;

/**
 * What a turn at work shows. Whether it waits or runs, and what a person
 * may do with it, come from the live read alone: the turns only say that it
 * is unfinished.
 * @param {Turn} turn
 * @param {Live | undefined} live
 * @param {Control | undefined} asked The control the person has pressed, until the turn ends.
 * @param {number} now Unix seconds.
 * @returns {Ledger}
 */
export function ledger(turn, live, asked, now) {
  const activity = live?.activity ?? undefined;
  const { running = [], recent = [], tool_calls: calls = 0, work, agent } = activity ?? {};
  const listed = recent.length + running.length;
  const tally = [
    ...(calls > 0 ? [`${String(calls)} tool ${calls === 1 ? "call" : "calls"}`] : []),
    span((now - turn.accepted_at) * 1000),
  ];
  const notes = [];
  if (work?.omitted !== undefined) notes.push(`${String(work.omitted)} more not shown.`);
  if (work?.partial === true) notes.push("The runtime could not see all of the delegated work.");
  if (activity?.partial === true) notes.push("Part of this report was left out for its size.");
  const model = [agent?.model, agent?.effort].filter(Boolean).join(" ");
  const context = agent?.context;
  const filled =
    context?.window === undefined
      ? ""
      : `context ${String(Math.round((context.used / context.window) * 100))}% full`;
  if (model !== "" || filled !== "") notes.push([model, filled].filter(Boolean).join(", "));
  const waits = live?.state === "queued";
  /** @type {Mark[]} */
  const past = Array.from({ length: Math.min(Math.max(calls - listed, 0), MAX_PAST_MARKS) });
  /** @type {Line[]} */
  const finished = recent.map((call) => ({
    mark: call.ok ? "done" : "failed",
    name: call.tool,
    detail: call.detail ?? "",
    time: timed(call.ok ? "" : "failed", call.duration_ms),
  }));
  /** @type {Line[]} */
  const underWay = running.map((call) => ({
    mark: "running",
    name: call.tool,
    detail: call.detail ?? "",
    time: `${call.estimated === true ? "about " : ""}${span((now - call.started_at) * 1000)}`,
  }));
  const offers = turn.from.kind === "person" && live !== undefined && live.state !== "ended";
  return {
    phase: phase(live),
    waiting: waits,
    tally: tally.join(", "),
    headline: activity?.headline ?? activity?.notes?.at(-1) ?? "",
    marks: [...past.fill("past"), ...[...finished, ...underWay].map(({ mark }) => mark)],
    calls: [...finished, ...underWay],
    work: (work?.items ?? []).map((item) => {
      const [mark, word] = WORK_STATES[item.state];
      const since = mark === "running" ? item.started_at : undefined;
      return {
        mark,
        name: WORK[item.kind],
        detail: item.actor === undefined ? item.label : `${item.label} (${item.actor})`,
        time: timed(word, since === undefined ? item.duration_ms : (now - since) * 1000),
      };
    }),
    notes,
    controls: (offers ? (activity?.controls ?? []) : []).map((action) => ({
      action,
      label: CONTROLS[action][asked === action ? "pressed" : "label"],
      title: CONTROLS[action][waits ? "waiting" : "answering"],
      asked: asked !== undefined,
    })),
  };
}

/**
 * The control the person pressed, while the page still shows it as pressed:
 * until the turn's live state moves on from what it was, or for a quarter
 * of a minute when the runtime took the control and changed nothing.
 * @param {{ action: Control, state: Live["state"] | undefined, at: number } | undefined} asked
 * @param {Live | undefined} live
 * @param {number} now Unix seconds.
 */
export function pressed(asked, live, now) {
  if (asked === undefined || asked.state !== live?.state || now - asked.at > PRESSED_FOR_SECONDS)
    return undefined;
  return asked.action;
}

/** @param {Live | undefined} live */
function phase(live) {
  if (live === undefined) return "At work";
  if (live.state === "ended") return "Finishing";
  if (live.state === "queued") return PHASES.queued;
  return live.activity === null ? "At work" : PHASES[live.activity.phase];
}

/**
 * @param {string} word
 * @param {number | undefined} ms
 */
function timed(word, ms) {
  return [word, ms === undefined ? "" : span(ms)].filter(Boolean).join(", ");
}

/**
 * How an ended turn ended, in words, and how long it took. The words come
 * from its state, never from its report's phase. A steered message has no
 * answer of its own, and which message got the answer is not known here.
 * @param {Turn} turn
 */
export function ending(turn) {
  const calls = turn.report?.tool_calls ?? 0;
  const seconds = (turn.finished_at ?? turn.accepted_at) - turn.accepted_at;
  const took = [
    ...(seconds > 0 ? [span(seconds * 1000)] : []),
    ...(calls > 0 ? [`${String(calls)} tool ${calls === 1 ? "call" : "calls"}`] : []),
  ].join(", ");
  if (turn.state === "queued" || turn.state === "running") return undefined;
  if (turn.state === "steered")
    return {
      label:
        "Steered into the answer that was running. The answer is with the newest message that took part.",
      took: "",
    };
  const label = { answered: "Answered", interrupted: "Did not finish", stopped: "Stopped" };
  return { label: label[turn.state], took };
}

/**
 * A conversation's name: its title, or else the first line the person wrote.
 * @param {Conversation | undefined} conversation
 * @param {Turn[]} turns
 */
export function titleOf(conversation, turns) {
  if (conversation !== undefined)
    return conversation.title === "" ? "Files only" : conversation.title;
  const [first] = turns;
  if (first === undefined) return "New conversation";
  const text = first.from.kind === "person" ? first.from.text : (first.from.label ?? "");
  const [line = ""] = text.trim().split(/\r?\n/u, 1);
  return line === "" ? "Files only" : line;
}

/**
 * A length of time, to two units.
 * @param {number} ms
 */
export function span(ms) {
  if (ms < 1000) return ms <= 0 ? "0 s" : `${String(Math.round(ms))} ms`;
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return `${String(seconds)} s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${String(minutes)} min ${String(seconds % 60)} s`;
  return `${String(Math.floor(minutes / 60))} h ${String(minutes % 60)} min`;
}

/**
 * A file's size, to two figures.
 * @param {number} bytes
 */
export function size(bytes) {
  const units = ["B", "kB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000;
    unit += 1;
  }
  return `${String(value < 10 && unit > 0 ? Math.round(value * 10) / 10 : Math.round(value))} ${units[unit] ?? "B"}`;
}

/**
 * A moment as a person says it: the time today, and the date before.
 * @param {number} at Unix seconds.
 * @param {number} now Unix seconds.
 * @param {string} [locale]
 */
export function when(at, now, locale) {
  const [date, today] = [new Date(at * 1000), new Date(now * 1000)];
  if (date.toDateString() === today.toDateString())
    return date.toLocaleTimeString(locale, { timeStyle: "short" });
  return date.toLocaleDateString(locale, {
    day: "numeric",
    month: "short",
    ...(date.getFullYear() === today.getFullYear() ? {} : { year: "numeric" }),
  });
}
