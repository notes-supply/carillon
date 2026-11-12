// @ts-check
/**
 * The page: what is on screen, the poll that keeps it true, and what the
 * person does. What it decides is in `model.js`, what it draws in `view.js`
 * and what it asks the provider in `api.js`.
 */
import { ask, data, put, token } from "./api.js";
import {
  AWAY,
  SIGNED_OUT,
  awaiting,
  changed,
  earlier,
  invokes,
  joined,
  merge,
  offered,
  overLimit,
  parseDraft,
  pressed,
  reading,
  refusal,
  restored,
  sequence,
  titleOf,
  unfinished,
  unread,
  wait,
} from "./model.js";
import { attachedFile, chat, el, pendingItem, phrase, turnBody, turnHead } from "./view.js";

/** @import { Answer } from "./api.js" */
/** @import { Command, Control, Conversation, Draft, Limits, Live, Page, Pending, Turn } from "./model.js" */

/**
 * An element `index.html` must have, so a copy of the page that lost one
 * says which.
 * @template {HTMLElement} T
 * @param {string} id
 * @param {new () => T} kind
 * @returns {T}
 */
function part(id, kind) {
  const found = document.getElementById(id);
  if (!(found instanceof kind)) throw new Error(`index.html has no ${kind.name} with ID ${id}`);
  return found;
}

const page = {
  app: part("app", HTMLElement),
  door: part("door", HTMLElement),
  doorWhy: part("door-why", HTMLElement),
  chats: part("chats", HTMLOListElement),
  noChats: part("no-chats", HTMLElement),
  moreChats: part("more-chats", HTMLButtonElement),
  custom: part("custom", HTMLElement),
  back: part("back", HTMLButtonElement),
  title: part("title", HTMLElement),
  agentName: part("with", HTMLElement),
  notice: part("notice", HTMLElement),
  transcript: part("transcript", HTMLElement),
  earlier: part("earlier", HTMLButtonElement),
  turns: part("turns", HTMLOListElement),
  invitation: part("invitation", HTMLElement),
  error: part("error", HTMLElement),
  menu: part("commands", HTMLElement),
  composer: part("composer", HTMLFormElement),
  attached: part("attached", HTMLElement),
  text: part("text", HTMLTextAreaElement),
  attach: part("attach", HTMLButtonElement),
  file: part("file", HTMLInputElement),
  agent: part("agent", HTMLSelectElement),
  send: part("send", HTMLButtonElement),
};

const UUID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/u;
/** @type {Page} */
const EMPTY = { turns: [], before: null };
const now = () => Math.floor(Date.now() / 1000);
const draftKey = (/** @type {string} */ id) => `draft:${id}`;

/** @type {{ agents: { id: string, name: string }[], limits: Limits, custom_site: boolean }} */
let session = {
  agents: [],
  limits: { files: 0, file_bytes: 0, message_bytes: 0 },
  custom_site: false,
};
/** The first page of conversations, as the last poll read it. @type {Conversation[]} */
let listed = [];
/** The pages after it that the person asked for, and where they continue. @type {Conversation[]} */
let older = [];
/** @type {number | null} */
let next = null;
/**
 * The conversation on screen. `stale` says its turns must be read, `seen`
 * is how the list had it when they last were, and `started` that core has it.
 * @type {{ id: string, page: Page, read: boolean, stale: boolean, started: boolean, seen: Conversation | undefined }}
 */
let open = { id: "", page: EMPTY, read: false, stale: true, started: false, seen: undefined };
/** The live read's turns, by ID. @type {Map<string, Live>} */
let live = new Map();
/** The control pressed on a turn. @type {Map<string, { action: Control, state: Live["state"] | undefined, at: number }>} */
const asked = new Map();
/** The message on its way in each conversation: one at a time. @type {Map<string, Pending>} */
const pending = new Map();
/** The files chosen for each conversation's next message. @type {Map<string, File[]>} */
const attached = new Map();
/** Why each conversation's last action was refused. @type {Map<string, string>} */
const errors = new Map();
/** @type {Map<string, Command[]>} */
const commands = new Map();
/** Each conversation's activity when the person last had it open, or when it was first listed. @type {Map<string, number>} */
const looked = new Map();
/** The turns drawn, so one that has not changed is left alone. @type {Map<string, { key: string, node: HTMLLIElement }>} */
const drawn = new Map();
/** The draft of a message that was being sent when the page last closed, until the person writes. @type {Draft | undefined} */
let unconfirmed;

const reads = sequence();
let [round, failures, timer, choice, railKey] = [0, 0, 0, 0, ""];
/** The person closed the command list, until they write again. */
let dismissed = false;
/** The transcript follows its end until the person scrolls away from it. */
let stick = true;
let signedOut = false;

/** @param {string} words */
function say(words) {
  page.notice.replaceChildren(...phrase(words));
  page.notice.hidden = words === "";
}

/**
 * Shows why a conversation's last action was refused, or with no words,
 * that nothing is wrong.
 * @param {string} id
 * @param {string} words
 */
function fail(id, words) {
  if (words === "") errors.delete(id);
  else errors.set(id, words);
  if (id !== open.id) return;
  page.error.replaceChildren(...phrase(words));
  page.error.hidden = words === "";
}

/** Closes the page to a browser that is not signed in. @param {string} [why] */
function shut(why = SIGNED_OUT) {
  signedOut = true;
  clearTimeout(timer);
  token.drop();
  page.app.hidden = true;
  page.door.hidden = false;
  page.doorWhy.replaceChildren(...phrase(why));
}

/**
 * Whether a read was answered. If not, says why and keeps what is on screen.
 * @param {Answer} answer
 */
function took(answer) {
  const kind = reading(answer.status);
  if (kind === "signed-out") shut();
  else if (kind !== "ok") say(refusal(answer.status, answer.body));
  return kind === "ok";
}

function kick() {
  void tick();
}

/** One poll, and the next one after it while the page is visible. */
async function tick() {
  const mine = ++round;
  clearTimeout(timer);
  const answered = await poll();
  if (mine !== round || signedOut) return;
  failures = answered ? 0 : failures + 1;
  if (answered) say("");
  if (!document.hidden) timer = setTimeout(kick, wait(failures));
}

async function poll() {
  const answer = await ask("/api/conversations");
  if (!took(answer)) return false;
  /** @type {{ conversations: Conversation[], next: number | null }} */
  const first = data(answer);
  listed = first.conversations;
  if (older.length === 0) next = first.next;
  drawRail();
  const current = listed.find(({ id }) => id === open.id);
  if ((open.stale || changed(open.seen, current)) && !(await readTurns(current))) return false;
  const ids = unfinished(open.page.turns);
  return ids.length === 0 || (await readLive(ids, current));
}

/**
 * Reads the open conversation's newest turns. `current` is how the list had
 * the conversation before the read began.
 * @param {Conversation | undefined} current
 */
async function readTurns(current) {
  const ticket = reads.next();
  const answer = await ask(`/api/conversations/${open.id}/turns`);
  if (!reads.current(ticket)) return true;
  // Core has no conversation before its first message: 404 is an empty one.
  if (answer.status !== 404) {
    if (!took(answer)) return false;
    open.page = merge(open.page, data(answer));
  }
  Object.assign(open, { started: answer.status !== 404, seen: current, stale: false, read: true });
  if (unconfirmed !== undefined) setText(restored(unconfirmed, open.page.turns));
  drawRail();
  drawTurns();
  drawComposer();
  return true;
}

/**
 * @param {string[]} ids
 * @param {Conversation | undefined} current
 */
async function readLive(ids, current) {
  const { id } = open;
  const answer = await ask(`/api/turns?ids=${ids.join(",")}`);
  if (id !== open.id) return true;
  if (!took(answer)) return false;
  /** @type {{ turns: Live[] }} */
  const read = data(answer);
  live = new Map(read.turns.map((turn) => [turn.id, turn]));
  if (read.turns.some((turn) => turn.state === "ended")) return await readTurns(current);
  drawTurns();
  return true;
}

function drawRail() {
  const all = [...listed, ...older.filter((old) => !listed.some(({ id }) => id === old.id))];
  for (const { id, activity } of all)
    if (!looked.has(id) || (id === open.id && !document.hidden)) looked.set(id, activity);
  const moment = now();
  const rows = all.map((conversation) => ({
    conversation,
    current: conversation.id === open.id,
    unread: unread(looked.get(conversation.id), conversation),
  }));
  const key = JSON.stringify([rows, Math.floor(moment / 60)]);
  if (key !== railKey) {
    railKey = key;
    const focused = document.activeElement;
    const hash =
      focused instanceof HTMLAnchorElement && page.chats.contains(focused) ? focused.hash : "";
    page.chats.replaceChildren(
      ...rows.map((row) => chat(row.conversation, { ...row, now: moment })),
    );
    const again = hash === "" ? null : page.chats.querySelector(`a[href="${hash}"]`);
    if (again instanceof HTMLElement) again.focus();
  }
  page.noChats.hidden = all.length > 0;
  page.moreChats.hidden = next === null;
  const current = all.find(({ id }) => id === open.id);
  const name = titleOf(current, open.page.turns);
  page.title.textContent = name;
  document.title = open.read && open.started ? name : "Carillon";
  const agent = session.agents.find(({ id }) => id === current?.agent_id);
  page.agentName.textContent =
    session.agents.length > 1 && agent !== undefined ? `with ${agent.name}` : "";
}

function drawTurns() {
  const moment = now();
  const nodes = open.page.turns.map((turn) => {
    const working = turn.state === "queued" || turn.state === "running";
    const read = live.get(turn.id);
    const control = working ? pressed(asked.get(turn.id), read, moment) : undefined;
    const key = JSON.stringify([turn, working ? [read, control, moment] : 0]);
    let entry = drawn.get(turn.id);
    if (entry === undefined) {
      entry = { key: "", node: el("li", "turn", turnHead(turn, moment), el("div")) };
      drawn.set(turn.id, entry);
    }
    if (entry.key !== key) {
      const { node } = entry;
      // A turn at work is drawn again every second: keep the keyboard on its control.
      const held = node.contains(document.activeElement)
        ? document.activeElement?.getAttribute("data-action")
        : undefined;
      const body = turnBody(turn, read, control, moment, (action) => void press(turn, action));
      node.lastElementChild?.replaceWith(body);
      entry.key = key;
      const again = held == null ? null : node.querySelector(`[data-action="${held}"]`);
      if (again instanceof HTMLElement) again.focus();
    }
    return entry.node;
  });
  const waiting = pending.get(open.id);
  if (waiting !== undefined && awaiting([waiting], open.page.turns).length > 0)
    nodes.push(pendingItem(waiting));
  else if (waiting?.stage === "received") pending.delete(open.id);
  nodes.forEach((node, index) => {
    const at = page.turns.children[index];
    if (at !== node) page.turns.insertBefore(node, at ?? null);
  });
  while (page.turns.children.length > nodes.length) page.turns.lastElementChild?.remove();
  page.earlier.hidden = open.page.before === null;
  page.invitation.hidden = !open.read || nodes.length > 0;
  if (stick) page.transcript.scrollTop = page.transcript.scrollHeight;
}

/** Whether the open conversation has a message that core has not received. */
function busy() {
  const waiting = pending.get(open.id);
  return waiting !== undefined && waiting.stage !== "received";
}

function drawComposer() {
  page.send.disabled = busy();
  // The agent is chosen with a conversation's first message, and fixed while that message is on its way.
  page.agent.hidden = !open.read || open.started || session.agents.length < 2;
  page.agent.disabled = busy();
  const chosen = attached.get(open.id) ?? [];
  page.attached.hidden = chosen.length === 0;
  page.attached.replaceChildren(
    ...chosen.map((file) =>
      attachedFile(file, () => {
        const rest = chosen.filter((other) => other !== file);
        attached.set(open.id, rest);
        fail(open.id, overLimit(rest, session.limits) ?? "");
        drawComposer();
      }),
    ),
  );
}

/** @param {string} value */
function setText(value) {
  page.text.value = value;
  page.text.style.height = "auto";
  page.text.style.height = `${String(page.text.scrollHeight)}px`;
}

/**
 * Stores a conversation's draft: what the composer holds and, until its
 * receipt, the text of the message on its way. No event ID and no file is
 * stored, so a reload sends nothing by itself.
 * @param {string} id
 * @param {string} text
 */
function store(id, text) {
  const waiting = pending.get(id);
  const sending =
    waiting === undefined || waiting.stage === "received"
      ? undefined
      : { text: waiting.text, at: waiting.at };
  if (text === "" && sending === undefined) localStorage.removeItem(draftKey(id));
  else localStorage.setItem(draftKey(id), JSON.stringify({ text, sending }));
}

/** @param {string} id */
function show(id) {
  reads.next();
  open = { id, page: EMPTY, read: false, stale: true, started: false, seen: undefined };
  live = new Map();
  drawn.clear();
  [railKey, stick, choice] = ["", true, 0];
  const draft = parseDraft(localStorage.getItem(draftKey(id)));
  unconfirmed = draft.sending !== undefined && !pending.has(id) ? draft : undefined;
  setText(unconfirmed === undefined ? draft.text : restored(unconfirmed, []));
  fail(id, errors.get(id) ?? "");
  drawRail();
  drawTurns();
  drawComposer();
  drawMenu();
  if (matchMedia("(min-width: 46rem)").matches) page.text.focus();
  kick();
}

/** Opens the conversation the address names, or a new one. */
function route() {
  const hash = location.hash.slice(1);
  if (hash.startsWith("sign-in=")) location.reload();
  else if (!signedOut) {
    const id = UUID.test(hash) ? hash : crypto.randomUUID();
    if (id !== hash) history.replaceState(null, "", `#${id}`);
    if (id !== open.id) show(id);
  }
}

function send() {
  const { id } = open;
  const text = page.text.value;
  const files = attached.get(id) ?? [];
  if (!open.read || busy() || (text.trim() === "" && files.length === 0)) return;
  const over = overLimit(files, session.limits);
  if (over !== undefined) {
    fail(id, over);
    return;
  }
  /** @type {Pending} */
  const message = {
    eventId: `m-${crypto.randomUUID()}`,
    agent: open.started ? null : page.agent.value,
    command: false,
    text,
    files,
    at: now(),
    stage: "uploading",
    sent: 0,
    total: 0,
  };
  pending.set(id, message);
  attached.delete(id);
  [unconfirmed, stick] = [undefined, true];
  setText("");
  store(id, "");
  fail(id, "");
  drawTurns();
  drawComposer();
  drawMenu();
  void deliver(id, message);
}

/**
 * Sends a message until core has it or refuses it. Every attempt is the
 * same request, so core admits it once however many arrive.
 * @param {string} id
 * @param {Pending} message
 */
async function deliver(id, message) {
  if (message.text.startsWith("/")) {
    await loadCommands(id, message.agent);
    message.command = invokes(message.text, commands.get(id) ?? []);
  }
  const query = new URLSearchParams({
    ...(message.agent === null ? {} : { agent: message.agent }),
    ...(message.command ? { command: "true" } : {}),
  });
  const path = `/api/conversations/${id}/messages/${message.eventId}?${query.toString()}`;
  const redraw = () => {
    if (id === open.id) drawTurns();
  };
  for (let failed = 0; ; failed += 1) {
    // A form of the page's own, so no empty `file` part is sent.
    const form = new FormData();
    form.set("text", message.text);
    for (const file of message.files) form.append("file", file);
    message.stage = "uploading";
    const answer = await put(path, form, (sent, total, done) => {
      Object.assign(message, { sent, total, stage: done ? "receiving" : "uploading" });
      redraw();
    });
    const kind = reading(answer.status);
    const typed = parseDraft(localStorage.getItem(draftKey(id))).text;
    if (kind === "ok") {
      message.stage = "received";
      store(id, typed);
      if (id === open.id) {
        open.stale = true;
        drawComposer();
        kick();
      }
      return;
    }
    if (kind !== "away") {
      // Refused: the message goes back before what was written since.
      pending.delete(id);
      store(id, joined(message.text, typed));
      attached.set(id, [...message.files, ...(attached.get(id) ?? [])]);
      fail(id, refusal(answer.status, answer.body));
      if (id === open.id) {
        setText(joined(message.text, typed));
        drawTurns();
        drawComposer();
      }
      if (kind === "signed-out") shut();
      return;
    }
    message.stage = "retrying";
    redraw();
    await new Promise((resolve) => setTimeout(resolve, wait(failed)));
  }
}

/**
 * @param {Turn} turn
 * @param {Control} action
 */
async function press(turn, action) {
  if (turn.from.kind !== "person") return;
  const { id } = open;
  asked.set(turn.id, { action, state: live.get(turn.id)?.state, at: now() });
  drawTurns();
  const answer = await ask(`/api/messages/${turn.from.event_id}/control`, "POST", { action });
  const kind = reading(answer.status);
  if (kind === "signed-out") shut();
  else if (kind === "ok") fail(id, "");
  else {
    asked.delete(turn.id);
    const unanswered = "Carillon did not answer. If the turn goes on, press it again.";
    fail(id, kind === "away" ? unanswered : refusal(answer.status, answer.body));
  }
  if (id === open.id) {
    open.stale = true;
    kick();
  }
}

/**
 * @param {string} id
 * @param {string | null} agent The agent, for a conversation core does not have yet.
 */
async function loadCommands(id, agent) {
  if (commands.has(id)) return;
  const query = agent === null ? "" : `?agent=${encodeURIComponent(agent)}`;
  const answer = await ask(`/api/conversations/${id}/commands${query}`);
  if (reading(answer.status) !== "ok") return;
  /** @type {{ commands: Command[] }} */
  const runtime = data(answer);
  commands.set(id, runtime.commands);
  if (id === open.id) drawMenu();
}

/** Draws the commands the composer's text could become, and returns them. */
function drawMenu() {
  const offers = offered(page.text.value, commands.get(open.id) ?? []);
  choice = Math.min(choice, Math.max(offers.length - 1, 0));
  page.menu.hidden = dismissed || offers.length === 0;
  if (dismissed) return [];
  page.menu.replaceChildren(
    ...offers.map((command, index) => {
      const what = [command.description, command.hint].filter(Boolean).join(" ");
      const item = el("li", "command", el("strong", "", `/${command.name}`), ` ${what}`);
      [item.role, item.ariaSelected] = ["option", String(index === choice)];
      // Before the composer loses the keyboard to the click.
      item.addEventListener("mousedown", (event) => {
        event.preventDefault();
        take(command);
      });
      return item;
    }),
  );
  return offers;
}

/** @param {Command} command */
function take(command) {
  setText(`/${command.name} `);
  wrote();
}

function wrote() {
  [unconfirmed, dismissed] = [undefined, false];
  setText(page.text.value);
  store(open.id, page.text.value);
  if (page.text.value.startsWith("/") && open.read)
    void loadCommands(open.id, open.started ? null : page.agent.value);
  drawMenu();
}

/** @param {File[]} files */
function attach(files) {
  if (files.length === 0) return;
  const chosen = [...(attached.get(open.id) ?? []), ...files];
  attached.set(open.id, chosen);
  fail(open.id, overLimit(chosen, session.limits) ?? "");
  drawComposer();
}

async function showEarlier() {
  const { id, page: held } = open;
  if (held.before === null) return;
  const answer = await ask(`/api/conversations/${id}/turns?before=${held.before}`);
  if (id !== open.id || !took(answer)) return;
  const height = page.transcript.scrollHeight;
  open.page = earlier(open.page, data(answer));
  stick = false;
  drawTurns();
  page.transcript.scrollTop += page.transcript.scrollHeight - height;
}

async function showMore() {
  if (next === null) return;
  const answer = await ask(`/api/conversations?before=${String(next)}`);
  if (!took(answer)) return;
  /** @type {{ conversations: Conversation[], next: number | null }} */
  const more = data(answer);
  [older, next] = [[...older, ...more.conversations], more.next];
  drawRail();
}

page.text.addEventListener("input", wrote);
page.text.addEventListener("keydown", (event) => {
  if (event.isComposing) return;
  const offers = drawMenu();
  const offer = offers[choice];
  const enter = event.key === "Enter" && !event.shiftKey;
  if (offer !== undefined && (event.key === "ArrowDown" || event.key === "ArrowUp")) {
    event.preventDefault();
    choice = (choice + (event.key === "ArrowDown" ? 1 : offers.length - 1)) % offers.length;
    drawMenu();
  } else if (offer !== undefined && event.key === "Escape") {
    dismissed = true;
    drawMenu();
  } else if (
    offer !== undefined &&
    (event.key === "Tab" || (enter && page.text.value !== `/${offer.name}`))
  ) {
    event.preventDefault();
    take(offer);
  } else if (enter) {
    event.preventDefault();
    send();
  }
});
page.text.addEventListener("paste", (event) => {
  const files = [...(event.clipboardData?.files ?? [])];
  if (files.length > 0) event.preventDefault();
  attach(files);
});
page.composer.addEventListener("submit", (event) => {
  event.preventDefault();
  send();
});
page.attach.addEventListener("click", () => page.file.click());
page.file.addEventListener("change", () => {
  attach([...(page.file.files ?? [])]);
  page.file.value = "";
});
page.agent.addEventListener("change", () => commands.delete(open.id));
document.addEventListener("dragover", (event) => event.preventDefault());
document.addEventListener("drop", (event) => {
  event.preventDefault();
  attach([...(event.dataTransfer?.files ?? [])]);
});
page.earlier.addEventListener("click", () => void showEarlier());
page.moreChats.addEventListener("click", () => void showMore());
page.transcript.addEventListener("scroll", () => {
  const { scrollHeight, scrollTop, clientHeight } = page.transcript;
  stick = scrollHeight - scrollTop - clientHeight < 48;
});
// An image that arrives changes the transcript's height. `load` does not bubble.
page.turns.addEventListener("load", () => drawTurns(), true);
// At phone width the rail and the conversation are two screens.
page.back.addEventListener("click", () => page.app.classList.remove("in-room"));
page.chats.addEventListener("click", () => page.app.classList.add("in-room"));
part("new", HTMLAnchorElement).addEventListener("click", () => page.app.classList.add("in-room"));
window.addEventListener("hashchange", route);
document.addEventListener("visibilitychange", () => {
  if (document.hidden) clearTimeout(timer);
  else if (!signedOut) kick();
});
// `carillon web open` signed this browser in from another tab.
window.addEventListener("storage", (event) => {
  if (signedOut && event.key === "token" && event.newValue !== null) location.reload();
});

async function start() {
  const code = new URLSearchParams(location.hash.slice(1)).get("sign-in");
  if (code !== null) {
    // The code works once, so it leaves the address before it is traded.
    history.replaceState(null, "", location.pathname);
    const traded = await ask("/api/sign-in", "POST", { code });
    if (reading(traded.status) !== "ok") {
      const unanswered = "Carillon did not answer the sign-in link. Run `carillon web open` again.";
      shut(reading(traded.status) === "away" ? unanswered : refusal(traded.status, traded.body));
      return;
    }
    /** @type {{ token: string }} */
    const signedIn = data(traded);
    token.set(signedIn.token);
  }
  for (let failed = 0; ; failed += 1) {
    const answer = await ask("/api/session");
    const kind = reading(answer.status);
    if (kind === "ok") {
      session = data(answer);
      break;
    }
    if (kind !== "away") {
      shut(kind === "signed-out" ? SIGNED_OUT : refusal(answer.status, answer.body));
      return;
    }
    page.door.hidden = false;
    page.doorWhy.replaceChildren(...phrase(AWAY));
    await new Promise((resolve) => setTimeout(resolve, wait(failed)));
  }
  page.door.hidden = true;
  page.app.hidden = false;
  page.custom.hidden = !session.custom_site;
  page.agent.replaceChildren(...session.agents.map((agent) => new Option(agent.name, agent.id)));
  route();
}

void start();
