// @ts-check
/**
 * The page's elements, built from the model's descriptions. Every string is
 * inserted as text. The one exception is `prose`, for HTML the provider
 * rendered from Markdown and sanitized.
 */
import { ending, ledger, plain, sendingLine, size, titleOf, when } from "./model.js";

/** @import { Control, Conversation, FileLink, Ledger, Line, Live, Mark, Pending, Turn } from "./model.js" */

/**
 * @template {keyof HTMLElementTagNameMap} K
 * @param {K} tag
 * @param {string} [className]
 * @param {(Node | string)[]} children
 */
export function el(tag, className = "", ...children) {
  const node = document.createElement(tag);
  if (className !== "") node.className = className;
  node.append(...children);
  return node;
}

/**
 * A sentence as nodes, with each part between backticks as code: the way
 * the page and the provider both write a command to run.
 * @param {string} words
 */
export function phrase(words) {
  return words.split("`").map((piece, index) => (index % 2 === 1 ? el("code", "", piece) : piece));
}

/** How long a mark under way takes to strike, and the ring to sound once. The stylesheet names the motions. */
const STRIKE_MS = 1100;
const SOUND_MS = 1800;

const LEGEND =
  "One mark for each tool call: a dot for a call no longer listed, a bar for one that finished, a short bar for one that failed, a hollow bar for one under way. The ring is the turn itself.";

/**
 * @param {(Mark | "now" | "now sounding")[]} marks
 * @param {string} label What the marks count, for whoever cannot see them.
 * @param {string} [title]
 */
function row(marks, label, title = "") {
  const node = el("span", "row");
  [node.role, node.ariaLabel, node.title] = ["img", label, title];
  marks.forEach((mark, index) => {
    const drawn = el("i", `mark ${mark}`);
    // A mark drawn again goes on where it was, each a half strike after the one before it.
    const period = mark === "running" ? 2 * STRIKE_MS : SOUND_MS;
    drawn.style.animationDuration = `${String(mark === "running" ? STRIKE_MS : SOUND_MS)}ms`;
    drawn.style.animationDelay = `${String(-((Date.now() + (index * STRIKE_MS) / 2) % period))}ms`;
    node.append(drawn);
  });
  return node;
}

/**
 * @param {number} at
 * @param {number} now
 */
function time(at, now) {
  const node = el("time", "", when(at, now));
  node.dateTime = new Date(at * 1000).toISOString();
  node.title = new Date(at * 1000).toLocaleString();
  return node;
}

/**
 * @param {FileLink[]} list
 * @param {boolean} inline
 */
function files(list, inline) {
  return el(
    "ul",
    "files",
    ...list.map((file) => {
      const image = file.type.startsWith("image/");
      const link = el("a", "file");
      link.href = file.url;
      if (image) [link.target, link.rel] = ["_blank", "noopener"];
      else link.download = file.name;
      if (!image || !inline) {
        link.append(el("span", "", file.name), el("span", "file-size", size(file.size)));
        return el("li", "", link);
      }
      const picture = el("img");
      [picture.src, picture.alt, picture.loading] = [file.url, file.name, "lazy"];
      link.className = "";
      link.append(picture);
      const caption = el("figcaption", "", `${file.name}, ${size(file.size)}`);
      return el("li", "picture", el("figure", "", link, caption));
    }),
  );
}

/**
 * @param {string} html
 * @param {string} className
 */
function prose(html, className) {
  const node = el("div", className);
  node.innerHTML = html;
  return node;
}

/**
 * Who began a turn: what the person wrote, or what the runtime opened it for.
 * @param {Turn} turn
 * @param {number} now
 */
export function turnHead(turn, now) {
  const { from } = turn;
  if (from.kind === "runtime")
    return el(
      "p",
      "opened",
      el("strong", "", from.label ?? "A turn of its own"),
      " Opened by Carillon, not by a message. ",
      time(turn.accepted_at, now),
    );
  const foot = el("div", "said-foot", time(turn.accepted_at, now));
  if (from.command) foot.append("Command");
  return el(
    "div",
    "said",
    el("p", "said-text", plain(from.text)),
    ...(from.files.length > 0 ? [files(from.files, false)] : []),
    foot,
  );
}

/** @param {Line[]} lines */
function lines(lines) {
  return el(
    "ol",
    "calls",
    ...lines.map(({ mark, name, detail, time: timed }) =>
      el(
        "li",
        "call",
        row([mark], mark),
        el("span", "call-name", name),
        el("span", "call-detail", detail),
        el("span", mark === "failed" ? "call-time failed" : "call-time", timed),
      ),
    ),
  );
}

/**
 * @param {Ledger} shown
 * @param {(action: Control) => void} control
 */
function ledgerNode(shown, control) {
  const head = el(
    "div",
    "ledger-head",
    row([...shown.marks, shown.waiting ? "now" : "now sounding"], shown.counted, LEGEND),
    el("span", "phase", shown.phase),
    el("span", "tally", shown.tally),
  );
  for (const offer of shown.controls) {
    const button = el("button", "button", offer.label);
    [button.type, button.title, button.disabled] = ["button", offer.title, offer.asked];
    button.dataset["action"] = offer.action;
    button.addEventListener("click", () => control(offer.action));
    head.append(button);
  }
  const node = el("section", "ledger", head);
  node.ariaLabel = "This turn is at work";
  if (shown.headline !== "") node.append(el("p", "headline", shown.headline));
  for (const part of [shown.calls, shown.work]) if (part.length > 0) node.append(lines(part));
  node.append(...shown.notes.map((note) => el("p", "ledger-note", note)));
  return node;
}

/**
 * How a turn stands: its ledger while it is at work, and then its answer,
 * its explanation or a line saying how it ended.
 * @param {Turn} turn
 * @param {Live | undefined} live
 * @param {Control | undefined} asked
 * @param {number} now
 * @param {(action: Control) => void} control
 */
export function turnBody(turn, live, asked, now, control) {
  const ended = ending(turn);
  if (ended === undefined) return ledgerNode(ledger(turn, live, asked, now), control);
  // What a finished turn did stays with it, folded behind the line that says how it ended.
  const outcome = el(ended.did.length > 0 ? "summary" : "p", "outcome", ended.label);
  if (turn.state !== "steered")
    outcome.append(" ", time(turn.finished_at ?? turn.accepted_at, now));
  if (ended.took !== "") outcome.append(`, ${ended.took}`);
  const foot = ended.did.length > 0 ? el("details", "did", outcome, lines(ended.did)) : outcome;
  if ((turn.report?.tool_calls ?? 0) > ended.did.length && ended.did.length > 0)
    foot.append(el("p", "ledger-note", "The calls before these are not listed."));
  const body = el("div", "turn-body");
  if (turn.state === "answered") {
    body.append(prose(turn.html, "prose"));
    if (turn.files.length > 0) body.append(files(turn.files, true));
  }
  body.append(foot);
  if (turn.state === "interrupted") {
    body.classList.add("interrupted");
    body.append(prose(turn.html, "explanation"));
  }
  return body;
}

/**
 * A message core has not said it received, drawn where its turn will be.
 * @param {Pending} pending
 */
export function pendingItem(pending) {
  const foot = el("div", "said-foot", sendingLine(pending));
  foot.role = "status";
  if (pending.stage === "uploading" && pending.files.length > 0) {
    const bar = el("progress");
    [bar.max, bar.value] = [Math.max(pending.total, 1), pending.sent];
    foot.append(bar);
  }
  const chips = pending.files.map((file) =>
    el("li", "file", el("span", "", file.name), el("span", "file-size", size(file.size))),
  );
  return el(
    "li",
    "turn",
    el(
      "div",
      "said pending",
      el("p", "said-text", pending.text),
      ...(chips.length > 0 ? [el("ul", "files", ...chips)] : []),
      foot,
    ),
  );
}

/**
 * A file chosen for the next message, with a button that takes it out.
 * @param {File} file
 * @param {() => void} remove
 */
export function attachedFile(file, remove) {
  const button = el("button", "", "×");
  button.type = "button";
  button.ariaLabel = `Remove ${file.name}`;
  button.addEventListener("click", remove);
  return el(
    "li",
    "file",
    el("span", "", file.name),
    el("span", "file-size", size(file.size)),
    button,
  );
}

/**
 * A conversation in the rail: its marks while it is at work, or a dot when
 * something arrived that the person has not looked at.
 * @param {Conversation} conversation
 * @param {{ current: boolean, unread: boolean, now: number }} how
 */
export function chat(conversation, { current, unread, now }) {
  const link = el(
    "a",
    "chat",
    el("span", "chat-title", titleOf(conversation, [])),
    el("span", "chat-when", when(conversation.active_at, now)),
  );
  link.href = `#${conversation.id}`;
  if (current) link.ariaCurrent = "page";
  if (conversation.at_work > 0) {
    link.append(row(["running", "running", "running"], "At work"));
  } else if (unread) {
    const dot = el("i", "unread");
    [dot.role, dot.ariaLabel] = ["img", "New"];
    link.append(dot);
  }
  return el("li", "", link);
}
