// @ts-check
import { ask, data, put, token } from "./api.js";
import {
  AWAY,
  SIGNED_OUT,
  awaiting,
  changed,
  dropsToken,
  earlier,
  generations,
  invokes,
  joined,
  linksAged,
  merge,
  offered,
  overLimit,
  pressed,
  reading,
  refusal,
  since,
  titleOf,
  unfinished,
  unread,
  wait,
} from "./model.js";
import { attachedFile, chat, el, pendingItem, phrase, turnBody, turnHead } from "./view.js";

/** @import { Answer } from "./api.js" */
/** @import { Asked, Command, Control, Conversation, Limits, Live, Page, Pending, Turn } from "./model.js" */

/**
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
/** @type {Conversation[]} */
let listed = [];
/** @type {Conversation[]} */
let older = [];
/** @type {number | null} */
let next = null;
/**
 * @type {{ id: string, page: Page, loaded: boolean, stale: boolean, inCore: boolean, listedAtRead: Conversation | undefined, readAt: number }}
 */
let open = {
  id: "",
  page: EMPTY,
  loaded: false,
  stale: true,
  inCore: false,
  listedAtRead: undefined,
  readAt: 0,
};
/** @type {Map<string, Live>} */
let live = new Map();
/** @type {Map<string, Asked>} */
const asked = new Map();
/** @type {Map<string, Pending>} */
const pending = new Map();
/** @type {Map<string, File[]>} */
const attached = new Map();
/** @type {Map<string, string>} */
const errors = new Map();
/** @type {Map<string, Command[]>} */
const commands = new Map();
/** @type {Map<string, number>} */
const activityWhenLooked = new Map();
/** @type {Map<string, { head: string, body: string, node: HTMLLIElement }>} */
const drawn = new Map();

/** A poll's reads end when a newer poll begins, another conversation is shown, or the page hides. */
const polls = generations(ask);
/** A read of earlier turns ends with the visit that asked for it. */
const turnPages = generations(ask);
const chatPages = generations(ask);
let [failures, timer, choice, railKey] = [0, 0, 0, ""];
let openInRail = false;
let menuDismissed = false;
let followEnd = true;
let signedOut = false;

/** @param {string} words */
function say(words) {
  page.notice.replaceChildren(...phrase(words));
  page.notice.hidden = words === "";
}

/**
 * @param {string} id
 * @param {string} words
 */
function setError(id, words) {
  if (words === "") errors.delete(id);
  else errors.set(id, words);
  if (id !== open.id) return;
  page.error.replaceChildren(...phrase(words));
  page.error.hidden = words === "";
}

/**
 * Closes the page to a browser that is not signed in.
 * @param {string} why
 * @param {Answer} [refused] The answer that said so, whose token is then dropped.
 */
function shut(why, refused) {
  signedOut = true;
  clearTimeout(timer);
  if (refused !== undefined && dropsToken(refused.status, refused.token, token.get())) token.drop();
  page.app.hidden = true;
  page.door.hidden = false;
  page.doorWhy.replaceChildren(...phrase(why));
}

/**
 * @param {Answer} answer
 */
function okOrNotice(answer) {
  const kind = reading(answer.status);
  if (kind === "signed-out") shut(SIGNED_OUT, answer);
  else if (kind !== "ok") say(refusal(answer.status, answer.body));
  return kind === "ok";
}

function focusComposer() {
  if (matchMedia("(min-width: 46rem)").matches) page.text.focus();
}

/** Polls now, unless the page is hidden: it polls at once when it shows again. */
function kick() {
  if (!document.hidden) void tick();
}

const pause = (/** @type {number} */ ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function tick() {
  clearTimeout(timer);
  const answered = await poll(polls.begin());
  if (answered === undefined || signedOut) return;
  failures = answered ? 0 : failures + 1;
  if (answered) say("");
  timer = setTimeout(kick, wait(failures));
}

/**
 * One poll: the list, then the open conversation's turns when the list
 * says they changed, then the live state of its unfinished turns.
 * @param {(path: string) => Promise<Answer | undefined>} read This poll's requests.
 * @returns {Promise<boolean | undefined>} Whether Carillon answered, or undefined once the poll has ended.
 */
async function poll(read) {
  const answer = await read("/api/conversations");
  if (answer === undefined) return undefined;
  if (!okOrNotice(answer)) return false;
  /** @type {{ conversations: Conversation[], next: number | null }} */
  const first = data(answer);
  listed = first.conversations;
  if (older.length === 0) next = first.next;
  drawRail();
  const current = listed.find(({ id }) => id === open.id);
  if (open.inCore && linksAged(open.readAt, now())) open.stale = true;
  if (open.stale || changed(open.listedAtRead, current)) {
    const turns = await readTurns(current, read);
    if (turns !== true) return turns;
  }
  const ids = unfinished(open.page.turns);
  if (ids.length === 0) return true;
  const lives = await read(`/api/turns?ids=${ids.join(",")}`);
  if (lives === undefined) return undefined;
  if (!okOrNotice(lives)) return false;
  /** @type {{ turns: Live[] }} */
  const { turns } = data(lives);
  live = new Map(turns.map((turn) => [turn.id, turn]));
  if (turns.some((turn) => turn.state === "ended")) return await readTurns(current, read);
  drawTurns();
  return true;
}

/**
 * @param {Conversation | undefined} current How the list had the conversation before the read began.
 * @param {(path: string) => Promise<Answer | undefined>} read
 */
async function readTurns(current, read) {
  const answer = await read(`/api/conversations/${open.id}/turns`);
  if (answer === undefined) return undefined;
  const inCore = answer.status !== 404;
  if (inCore) {
    if (!okOrNotice(answer)) return false;
    open.page = merge(open.page, data(answer));
  }
  Object.assign(open, { inCore, listedAtRead: current, stale: false, loaded: true, readAt: now() });
  drawRail();
  drawTurns();
  drawComposer();
  return true;
}

/**
 * @param {HTMLElement} container
 * @param {string} attribute What names the focused element, before the redraw and after it.
 * @param {() => void} redraw
 */
function keepingFocus(container, attribute, redraw) {
  const focused = document.activeElement;
  const name = container.contains(focused) ? focused?.getAttribute(attribute) : null;
  redraw();
  const again = name == null ? null : container.querySelector(`[${attribute}="${name}"]`);
  if (again instanceof HTMLElement) again.focus();
}

function drawRail() {
  const all = [...listed, ...older.filter((old) => !listed.some(({ id }) => id === old.id))];
  for (const { id, activity } of all)
    if (!activityWhenLooked.has(id) || (id === open.id && !document.hidden))
      activityWhenLooked.set(id, activity);
  const moment = now();
  const rows = all.map((conversation) => ({
    conversation,
    current: conversation.id === open.id,
    unread: unread(activityWhenLooked.get(conversation.id), conversation),
  }));
  const key = JSON.stringify([rows, Math.floor(moment / 60)]);
  if (key !== railKey) {
    railKey = key;
    keepingFocus(page.chats, "href", () =>
      page.chats.replaceChildren(
        ...rows.map((row) => chat(row.conversation, { ...row, now: moment })),
      ),
    );
  }
  page.noChats.hidden = all.length > 0;
  page.moreChats.hidden = next === null;
  const current = all.find(({ id }) => id === open.id);
  if (current !== undefined && !openInRail)
    page.chats.querySelector("[aria-current]")?.scrollIntoView({ block: "nearest" });
  openInRail = current !== undefined;
  const name = titleOf(current, open.page.turns);
  page.title.textContent = name;
  document.title = open.loaded && open.inCore ? name : "Carillon";
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
    const [head, body] = [
      JSON.stringify(turn.from),
      JSON.stringify([turn, working ? [read, control, moment] : 0]),
    ];
    let entry = drawn.get(turn.id);
    if (entry === undefined) {
      entry = { head: "", body: "", node: el("li", "turn", el("div"), el("div")) };
      entry.node.tabIndex = -1;
      drawn.set(turn.id, entry);
    }
    const { node } = entry;
    // A turn read again has new file links, the person's own files included.
    if (entry.head !== head) node.firstElementChild?.replaceWith(turnHead(turn, moment));
    if (entry.body !== body)
      keepingFocus(node, "data-action", () =>
        node.lastElementChild?.replaceWith(
          turnBody(turn, read, control, moment, (action) => void press(turn, action)),
        ),
      );
    Object.assign(entry, { head, body });
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
  page.invitation.hidden = !open.loaded || nodes.length > 0;
  // Follow the end, but show a newest turn taller than the window from its top.
  const [newest, { transcript }] = [nodes.at(-1), page];
  if (followEnd && newest !== undefined)
    transcript.scrollTop =
      newest.offsetHeight > transcript.clientHeight
        ? newest.offsetTop - transcript.offsetTop
        : transcript.scrollHeight;
}

function busy() {
  const waiting = pending.get(open.id);
  return waiting !== undefined && waiting.stage !== "received";
}

function drawComposer() {
  page.send.disabled = busy();
  const choosesAgent = open.loaded && !open.inCore && session.agents.length > 1;
  page.agent.hidden = !choosesAgent;
  page.agent.disabled = busy();
  const chosen = attached.get(open.id) ?? [];
  page.attached.hidden = chosen.length === 0;
  page.attached.replaceChildren(
    ...chosen.map((file) =>
      attachedFile(file, () => {
        const rest = chosen.filter((other) => other !== file);
        attached.set(open.id, rest);
        setError(open.id, overLimit(rest, session.limits) ?? "");
        drawComposer();
        (page.attached.querySelectorAll("button")[chosen.indexOf(file)] ?? page.text).focus();
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
 * @param {string} id
 * @param {string} text
 */
function storeDraft(id, text) {
  const waiting = pending.get(id);
  const whole =
    waiting === undefined || waiting.stage === "received" ? text : joined(waiting.text, text);
  if (whole === "") localStorage.removeItem(draftKey(id));
  else localStorage.setItem(draftKey(id), whole);
}

/**
 * @param {string} id
 * @param {boolean} minted
 */
function show(id, minted) {
  polls.end();
  turnPages.end();
  open = {
    id,
    page: EMPTY,
    loaded: minted,
    stale: !minted,
    inCore: false,
    listedAtRead: undefined,
    readAt: 0,
  };
  live = new Map();
  drawn.clear();
  [railKey, followEnd, choice, openInRail] = ["", true, 0, false];
  const draft = localStorage.getItem(draftKey(id)) ?? "";
  setText(busy() ? since(draft, pending.get(id)?.text ?? "") : draft);
  setError(id, errors.get(id) ?? "");
  drawRail();
  drawTurns();
  drawComposer();
  drawMenu();
  focusComposer();
  kick();
}

function route() {
  const hash = location.hash.slice(1);
  if (hash.startsWith("sign-in=")) location.reload();
  else if (!signedOut) {
    const id = UUID.test(hash) ? hash : crypto.randomUUID();
    if (id !== hash) history.replaceState(null, "", `#${id}`);
    if (id !== open.id) show(id, id !== hash);
  }
}

function send() {
  const { id } = open;
  const text = page.text.value;
  const files = attached.get(id) ?? [];
  if (!open.loaded || busy() || (text.trim() === "" && files.length === 0)) return;
  const over = overLimit(files, session.limits);
  if (over !== undefined) {
    setError(id, over);
    return;
  }
  /** @type {Pending} */
  const message = {
    eventId: `m-${crypto.randomUUID()}`,
    agent: open.inCore ? null : page.agent.value,
    command: false,
    text,
    files,
    stage: "uploading",
    sent: 0,
    total: 0,
  };
  pending.set(id, message);
  attached.delete(id);
  followEnd = true;
  // The stored draft is left as it is: it holds this message until its receipt.
  setText("");
  setError(id, "");
  drawTurns();
  drawComposer();
  drawMenu();
  void deliver(id, message);
}

/**
 * @param {string} id
 * @param {Pending} message
 */
async function deliver(id, message) {
  const redraw = () => {
    if (id === open.id) drawTurns();
  };
  if (message.text.startsWith("/")) {
    // Whether a text is a command is the runtime's to say, so the message waits for its list.
    for (let failed = 0; !(await loadCommands(id, message.agent)); failed += 1) {
      message.stage = "retrying";
      redraw();
      await pause(wait(failed));
    }
    message.command = invokes(message.text, commands.get(id) ?? []);
  }
  const query = [
    ...(message.agent === null ? [] : [`agent=${encodeURIComponent(message.agent)}`]),
    ...(message.command ? ["command=true"] : []),
  ];
  const path = `/api/conversations/${id}/messages/${message.eventId}${query.length > 0 ? "?" : ""}${query.join("&")}`;
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
    const stored = localStorage.getItem(draftKey(id)) ?? "";
    if (kind === "ok") {
      message.stage = "received";
      // Only what was sent is cleared: another tab may have stored a draft of its own since.
      storeDraft(id, since(stored, message.text));
      if (id === open.id) {
        open.stale = true;
        drawComposer();
        kick();
      }
      return;
    }
    if (kind !== "away") {
      // Refused: the message goes back to the composer, before what was written since.
      pending.delete(id);
      attached.set(id, [...message.files, ...(attached.get(id) ?? [])]);
      setError(id, refusal(answer.status, answer.body));
      const typed = id === open.id ? page.text.value : since(stored, message.text);
      storeDraft(id, joined(message.text, typed));
      if (id === open.id) {
        setText(joined(message.text, typed));
        drawTurns();
        drawComposer();
      }
      if (kind === "signed-out") shut(SIGNED_OUT, answer);
      return;
    }
    message.stage = "retrying";
    redraw();
    await pause(wait(failed));
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
  focusComposer();
  const answer = await ask(`/api/messages/${turn.from.event_id}/control`, "POST", { action });
  const kind = reading(answer.status);
  if (kind === "signed-out") shut(SIGNED_OUT, answer);
  else if (kind === "ok") setError(id, "");
  else {
    asked.delete(turn.id);
    const unanswered = "Carillon did not answer. If the turn goes on, press it again.";
    setError(id, kind === "away" ? unanswered : refusal(answer.status, answer.body));
  }
  if (id === open.id) {
    open.stale = true;
    kick();
  }
}

/**
 * @param {string} id
 * @param {string | null} agent The agent, for a conversation core does not have yet.
 * @returns {Promise<boolean>} Whether the runtime said: its list, or a refusal a send would get too.
 */
async function loadCommands(id, agent) {
  if (commands.has(id)) return true;
  const query = agent === null ? "" : `?agent=${encodeURIComponent(agent)}`;
  const answer = await ask(`/api/conversations/${id}/commands${query}`);
  const kind = reading(answer.status);
  if (kind !== "ok") return kind !== "away";
  /** @type {{ commands: Command[] }} */
  const runtime = data(answer);
  commands.set(id, runtime.commands);
  if (id === open.id) drawMenu();
  return true;
}

function offers() {
  return menuDismissed ? [] : offered(page.text.value, commands.get(open.id) ?? []);
}

function drawMenu() {
  const listed = offers();
  choice = Math.min(choice, Math.max(listed.length - 1, 0));
  page.menu.hidden = listed.length === 0;
  page.menu.replaceChildren(
    ...listed.map((command, index) => {
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
}

/** @param {Command} command */
function take(command) {
  setText(`/${command.name} `);
  wrote();
}

function wrote() {
  menuDismissed = false;
  setText(page.text.value);
  storeDraft(open.id, page.text.value);
  if (page.text.value.startsWith("/") && open.loaded)
    void loadCommands(open.id, open.inCore ? null : page.agent.value);
  drawMenu();
}

/** @param {File[]} files */
function attach(files) {
  if (files.length === 0) return;
  const chosen = [...(attached.get(open.id) ?? []), ...files];
  attached.set(open.id, chosen);
  setError(open.id, overLimit(chosen, session.limits) ?? "");
  drawComposer();
}

async function showEarlier() {
  const { before } = open.page;
  if (before === null) return;
  const answer = await turnPages.begin()(`/api/conversations/${open.id}/turns?before=${before}`);
  // A newest page read meanwhile may have replaced the turns this page continues.
  if (answer === undefined || open.page.before !== before || !okOrNotice(answer)) return;
  const height = page.transcript.scrollHeight;
  open.page = earlier(open.page, data(answer));
  followEnd = false;
  drawTurns();
  page.transcript.scrollTop += page.transcript.scrollHeight - height;
  const first = page.turns.firstElementChild;
  if (first instanceof HTMLElement) first.focus({ preventScroll: true });
}

async function showMore() {
  if (next === null) return;
  const answer = await chatPages.begin()(`/api/conversations?before=${String(next)}`);
  if (answer === undefined || !okOrNotice(answer)) return;
  /** @type {{ conversations: Conversation[], next: number | null }} */
  const more = data(answer);
  const shown = page.chats.children.length;
  [older, next] = [[...older, ...more.conversations], more.next];
  drawRail();
  const first = page.chats.children[shown]?.firstElementChild;
  if (first instanceof HTMLElement) first.focus();
}

page.text.addEventListener("input", wrote);
page.text.addEventListener("keydown", (event) => {
  if (event.isComposing) return;
  const listed = offers();
  const offer = listed[choice];
  const enter = event.key === "Enter" && !event.shiftKey;
  if (offer !== undefined && (event.key === "ArrowDown" || event.key === "ArrowUp")) {
    event.preventDefault();
    choice = (choice + (event.key === "ArrowDown" ? 1 : listed.length - 1)) % listed.length;
    drawMenu();
  } else if (offer !== undefined && event.key === "Escape") {
    menuDismissed = true;
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
  followEnd = scrollHeight - scrollTop - clientHeight < 48;
});
// An image that arrives changes the transcript's height. `load` does not bubble.
page.turns.addEventListener("load", () => drawTurns(), true);
page.back.addEventListener("click", () => page.app.classList.remove("in-room"));
page.chats.addEventListener("click", () => page.app.classList.add("in-room"));
part("new", HTMLAnchorElement).addEventListener("click", () => page.app.classList.add("in-room"));
window.addEventListener("hashchange", route);
document.addEventListener("visibilitychange", () => {
  if (document.hidden) {
    clearTimeout(timer);
    polls.end();
  } else if (!signedOut) kick();
});
window.addEventListener("storage", (event) => {
  if (signedOut && event.key === "token" && event.newValue !== null) location.reload();
  // Another tab cleared this conversation's draft, as its own receipt does: what this tab holds is still unsent.
  if (event.key === draftKey(open.id) && event.newValue === null)
    storeDraft(open.id, page.text.value);
});

/** The sign-in code of the address, which it leaves: a code works once. */
function takeSignInCode() {
  const code = new URLSearchParams(location.hash.slice(1)).get("sign-in");
  if (code !== null) history.replaceState(null, "", location.pathname);
  return code;
}

async function start() {
  const code = takeSignInCode();
  if (code !== null) {
    const traded = await ask("/api/sign-in", "POST", { code });
    const kind = reading(traded.status);
    if (kind === "ok") {
      /** @type {{ token: string }} */
      const signedIn = data(traded);
      token.set(signedIn.token);
    } else if (token.get() === null) {
      const unanswered = "Carillon did not answer the sign-in link. Run `carillon web open` again.";
      shut(kind === "away" ? unanswered : refusal(traded.status, traded.body));
      return;
    }
    // A link that no longer works changes nothing for a browser that is signed in.
  }
  for (let failed = 0; ; failed += 1) {
    const answer = await ask("/api/session");
    const kind = reading(answer.status);
    if (kind === "ok") {
      session = data(answer);
      break;
    }
    if (kind !== "away") {
      shut(kind === "signed-out" ? SIGNED_OUT : refusal(answer.status, answer.body), answer);
      return;
    }
    page.door.hidden = false;
    page.doorWhy.replaceChildren(...phrase(AWAY));
    await pause(wait(failed));
  }
  page.door.hidden = true;
  page.app.hidden = false;
  page.custom.hidden = !session.custom_site;
  page.agent.replaceChildren(...session.agents.map((agent) => new Option(agent.name, agent.id)));
  route();
}

void start();
