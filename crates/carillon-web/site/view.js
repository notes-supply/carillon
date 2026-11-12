// @ts-check
/**
 * The page's elements, built from the model's descriptions. Every string is
 * inserted as text. The one exception is `prose`, for HTML the provider
 * rendered from Markdown and sanitized.
 */
import { ending, ledger, plain, sendingLine, size, when } from "./model.js";

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

/** One turn of both of the marks' animations, so a mark drawn again goes on where it was. */
const CYCLE = 19_800;

/**
 * A row of marks, one for each call, read aloud as `label`.
 * @param {(Mark | "now" | "now sounding")[]} marks
 * @param {string} label
 */
function row(marks, label) {
  const node = el("span", "row");
  node.role = "img";
  node.ariaLabel = label;
  marks.forEach((mark, index) => {
    const drawn = el("i", `mark ${mark}`);
    drawn.style.animationDelay = `${String(-((Date.now() + index * 550) % CYCLE))}ms`;
    node.append(drawn);
  });
  return node;
}

/**
 * A moment, with the whole date for whoever asks.
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
 * Files as links. With `inline`, a raster image is drawn in place.
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
 * The provider's HTML for an answer or an explanation, inserted as it is.
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
 * The ledger of a turn at work, with its controls beside the phase.
 * @param {Ledger} shown
 * @param {(action: Control) => void} control
 */
function ledgerNode(shown, control) {
  const head = el(
    "div",
    "ledger-head",
    row([...shown.marks, shown.waiting ? "now" : "now sounding"], shown.tally),
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
  const foot = el("p", "outcome", ended.label);
  if (ended.took !== "") {
    const at = turn.finished_at ?? turn.accepted_at;
    foot.append(" ", time(at, now), `, ${ended.took}`);
  }
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
    el("span", "chat-title", conversation.title === "" ? "Files only" : conversation.title),
    el("span", "chat-when", when(conversation.active_at, now)),
  );
  link.href = `#${conversation.id}`;
  if (current) link.ariaCurrent = "page";
  if (conversation.at_work > 0) {
    /** @type {Mark[]} */
    const marks = Array.from({ length: Math.min(conversation.at_work, 3) }, () => "running");
    link.append(row(marks, "At work"));
  } else if (unread) {
    const dot = el("i", "unread");
    [dot.role, dot.ariaLabel] = ["img", "New"];
    link.append(dot);
  }
  return el("li", "", link);
}
