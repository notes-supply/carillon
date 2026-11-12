// @ts-check
import { describe, expect, it } from "vitest";
import {
  AWAY,
  SIGNED_OUT,
  awaiting,
  changed,
  dropsToken,
  earlier,
  generations,
  ending,
  invokes,
  joined,
  ledger,
  linksAged,
  merge,
  offered,
  overLimit,
  plain,
  pressed,
  reading,
  refusal,
  sendingLine,
  since,
  size,
  span,
  titleOf,
  unfinished,
  unread,
  wait,
  when,
} from "../site/model.js";

/** @import { Activity, Conversation, Live, Pending, Turn } from "../site/model.js" */

/** @param {Partial<Conversation>} [fields] @returns {Conversation} */
const conversation = (fields) => ({
  id: "c1",
  agent_id: "assistant",
  title: "Plan the release",
  started_at: 100,
  active_at: 200,
  activity: 5,
  at_work: 0,
  ...fields,
});

/**
 * @param {string} id
 * @param {Turn["state"]} [state]
 * @param {string} [text]
 * @returns {Turn}
 */
function turn(id, state = "answered", text = "hello") {
  const base = {
    id,
    from: {
      kind: /** @type {const} */ ("person"),
      event_id: `m-${id}`,
      text,
      command: false,
      files: [],
    },
    accepted_at: 1000,
    finished_at: state === "queued" || state === "running" ? null : 1134,
    report: null,
  };
  if (state === "answered") return { ...base, state, html: "<p>hi</p>", files: [] };
  if (state === "interrupted") return { ...base, state, html: "<p>The model failed.</p>" };
  return { ...base, state };
}

/** @param {string} id @returns {Turn} */
const opened = (id) => ({
  ...turn(id, "running"),
  from: { kind: "runtime", label: "Scheduled run: morning summary" },
});

/** @param {Partial<Activity>} [fields] @returns {Activity} */
const activity = (fields) => ({
  phase: "tools",
  running: [],
  recent: [],
  tool_calls: 0,
  ...fields,
});

/** @param {Live["state"]} state @param {Partial<Activity> | null} [fields] @returns {Live} */
const live = (state, fields = {}) => ({
  id: "t1",
  state,
  activity: fields === null ? null : activity(fields),
});

/** @param {Partial<Pending>} [fields] @returns {Pending} */
const pending = (fields) => ({
  eventId: "m-p1",
  agent: null,
  command: false,
  text: "hello",
  files: [],
  stage: "uploading",
  sent: 0,
  total: 0,
  ...fields,
});

describe("the poll", () => {
  it("reads a conversation again only when its activity or its turns at work changed", () => {
    expect(changed(conversation(), conversation())).toBe(false);
    expect(changed(conversation(), conversation({ active_at: 999, title: "x" }))).toBe(false);
    expect(changed(conversation(), conversation({ activity: 6 }))).toBe(true);
    expect(changed(conversation(), conversation({ at_work: 1 }))).toBe(true);
  });

  it("takes a conversation the first page does not list as unchanged", () => {
    expect(changed(conversation(), undefined)).toBe(false);
    expect(changed(undefined, undefined)).toBe(false);
  });

  it("reads a conversation it has not seen listed before", () => {
    expect(changed(undefined, conversation())).toBe(true);
  });

  it("marks a conversation whose activity is past what the person last looked at", () => {
    expect(unread(5, conversation())).toBe(false);
    expect(unread(4, conversation())).toBe(true);
    expect(unread(undefined, conversation())).toBe(false);
  });

  it("waits about a second, and longer after each failure, up to ten", () => {
    expect([0, 1, 2, 3, 4, 9].map(wait)).toEqual([1000, 2000, 4000, 8000, 10_000, 10_000]);
  });

  it("asks the live read for the newest unfinished turns, at most 32", () => {
    expect(
      unfinished([turn("a"), turn("b", "running"), turn("c", "stopped"), turn("d", "queued")]),
    ).toEqual(["b", "d"]);
    const many = Array.from({ length: 40 }, (_, index) => turn(String(index), "running"));
    expect(unfinished(many)).toHaveLength(32);
    expect(unfinished(many).at(-1)).toBe("39");
  });
});

describe("answers to requests", () => {
  /** A provider whose answers arrive when the test says, and which records what it was asked. */
  function provider() {
    /** @type {{ path: string, answer: (body: string) => void }[]} */
    const asked = [];
    /** @param {string} path @returns {Promise<string>} */
    const ask = (path) => new Promise((answer) => asked.push({ path, answer }));
    return { asked, ask, paths: () => asked.map(({ path }) => path) };
  }

  it("delivers the answers of the newest generation of requests", async () => {
    const { asked, ask } = provider();
    const read = generations(ask).begin();
    const list = read("/list");
    asked[0]?.answer("the list");
    await expect(list).resolves.toBe("the list");
    const turns = read("/turns");
    asked[1]?.answer("the turns");
    await expect(turns).resolves.toBe("the turns");
  });

  it("drops an answer that a newer request of its kind superseded", async () => {
    const { asked, ask } = provider();
    const polls = generations(ask);
    const older = polls.begin()("/list");
    const newer = polls.begin()("/list");
    asked[1]?.answer("the newer list");
    asked[0]?.answer("the older list, late");
    await expect(newer).resolves.toBe("the newer list");
    await expect(older).resolves.toBeUndefined();
  });

  it("starts no further request of a poll once the page hid", async () => {
    const { asked, ask, paths } = provider();
    const polls = generations(ask);
    const read = polls.begin();
    const list = read("/list");
    polls.end();
    asked[0]?.answer("the list");
    await expect(list).resolves.toBeUndefined();
    await expect(read("/turns")).resolves.toBeUndefined();
    await expect(read("/live")).resolves.toBeUndefined();
    expect(paths()).toEqual(["/list"]);
  });

  it("drops an earlier page asked for in a visit that has ended, and by an older click", async () => {
    const { asked, ask } = provider();
    const pages = generations(ask);
    const previousVisit = pages.begin()("/turns?before=a");
    pages.end();
    const firstClick = pages.begin()("/turns?before=b");
    const secondClick = pages.begin()("/turns?before=b");
    for (const [index, body] of ["of the old visit", "first", "second"].entries())
      asked[index]?.answer(body);
    await expect(previousVisit).resolves.toBeUndefined();
    await expect(firstClick).resolves.toBeUndefined();
    await expect(secondClick).resolves.toBe("second");
  });

  it("drops the stored token only on a 401 to a request that carried it", () => {
    expect(dropsToken(401, "t1", "t1")).toBe(true);
    // A sign-in link that failed carried no token, so it says nothing about the one stored.
    expect(dropsToken(401, null, "t1")).toBe(false);
    expect(dropsToken(401, null, null)).toBe(false);
    // Another tab signed in since: its token is not the one refused.
    expect(dropsToken(401, "t1", "t2")).toBe(false);
    for (const status of [0, 200, 403, 503]) expect(dropsToken(status, "t1", "t1")).toBe(false);
  });

  it("tells signed out, away and refused apart", () => {
    expect([200, 204].map(reading)).toEqual(["ok", "ok"]);
    expect([0, 503].map(reading)).toEqual(["away", "away"]);
    expect(reading(401)).toBe("signed-out");
    expect([400, 404, 409, 413, 422, 507].map(reading)).toEqual(Array(6).fill("refused"));
  });

  it("shows the provider's words when it gave any", () => {
    expect(refusal(409, '{"error":"This conversation is with another agent."}')).toBe(
      "This conversation is with another agent.",
    );
    expect(refusal(404, '{"error":"no agent named \\"x\\""}')).toBe('No agent named "x"');
  });

  it("has a sentence of its own for a status without words", () => {
    expect(refusal(0, "")).toBe(AWAY);
    expect(
      refusal(503, '{"error":"Carillon is not answering right now. Try again in a moment."}'),
    ).toBe(AWAY);
    expect(refusal(401, "")).toBe(SIGNED_OUT);
    expect(SIGNED_OUT).toContain("carillon web open");
    expect(refusal(422, "Failed to deserialize")).toBe(
      "Carillon refused the request with status 422.",
    );
    expect(refusal(404, '{"error":7}')).toBe("Carillon refused the request with status 404.");
  });
});

describe("turns", () => {
  const held = { turns: [turn("a"), turn("b", "running"), turn("c", "queued")], before: "a" };

  it("replaces held turns by ID and keeps older pages", () => {
    const page = { turns: [turn("b"), turn("c", "running"), turn("d", "queued")], before: "b" };
    const merged = merge(held, page);
    expect(merged.turns.map(({ id, state }) => [id, state])).toEqual([
      ["a", "answered"],
      ["b", "answered"],
      ["c", "running"],
      ["d", "queued"],
    ]);
    expect(merged.before).toBe("a");
  });

  it("takes a page that reaches the first turn as the whole conversation", () => {
    const page = { turns: [turn("b"), turn("c")], before: null };
    expect(merge(held, page)).toEqual(page);
  });

  it("replaces everything when the newest page shares no turn with those held", () => {
    const page = { turns: [turn("x"), turn("y")], before: "x" };
    expect(merge(held, page)).toEqual(page);
    expect(merge({ turns: [], before: null }, page)).toEqual(page);
  });

  it("puts an older page before the turns held", () => {
    const older = { turns: [turn("0"), turn("a")], before: null };
    const all = earlier(held, older);
    expect(all.turns.map(({ id }) => id)).toEqual(["0", "a", "b", "c"]);
    expect(all.before).toBeNull();
  });

  it("reads turns again on a later UTC day, before their file links stop working", () => {
    const at = (/** @type {string} */ iso) => Date.parse(iso) / 1000;
    expect(linksAged(at("2026-11-12T00:00:01Z"), at("2026-11-12T23:59:59Z"))).toBe(false);
    expect(linksAged(at("2026-11-12T23:59:59Z"), at("2026-11-13T00:00:00Z"))).toBe(true);
    expect(linksAged(at("2026-11-10T12:00:00Z"), at("2026-11-12T12:00:00Z"))).toBe(true);
  });

  it("names a conversation by its title or the first line written", () => {
    expect(titleOf(conversation(), [])).toBe("Plan the release");
    expect(titleOf(conversation({ title: "" }), [])).toBe("Files only");
    expect(titleOf(undefined, [])).toBe("New conversation");
    expect(titleOf(undefined, [turn("a", "answered", "\r\nChart this.\r\nAnd that.")])).toBe(
      "Chart this.",
    );
    expect(titleOf(undefined, [opened("a")])).toBe("Scheduled run: morning summary");
  });
});

describe("a message not yet received", () => {
  it("is drawn until its turn is on screen", () => {
    const message = pending({ eventId: "m-b" });
    expect(awaiting([message], [turn("a")])).toEqual([message]);
    expect(awaiting([message], [turn("a"), turn("b")])).toEqual([]);
    expect(awaiting([message], [opened("b")])).toEqual([message]);
  });

  it("says where it is without claiming to know why it waits", () => {
    expect(sendingLine(pending())).toBe("Sending");
    const file = new File(["x"], "a.txt");
    expect(sendingLine(pending({ files: [file], sent: 512, total: 1024 }))).toBe("Sending, 50%");
    expect(sendingLine(pending({ stage: "receiving" }))).toBe("Carillon is receiving it.");
    expect(sendingLine(pending({ stage: "received" }))).toBe("Received.");
    expect(sendingLine(pending({ stage: "retrying" }))).toBe(
      "Carillon is not answering. This message will be sent when it does.",
    );
  });

  it("is checked against core's limits before it is uploaded", () => {
    const limits = { files: 2, file_bytes: 1000, message_bytes: 1500 };
    const file = (/** @type {number} */ bytes) => ({ name: "a.bin", size: bytes });
    expect(overLimit([file(1000), file(500)], limits)).toBeUndefined();
    expect(overLimit([file(1), file(1), file(1)], limits)).toMatch(/at most 2 files/u);
    expect(overLimit([file(1001)], limits)).toMatch(/a\.bin is 1 kB/u);
    expect(overLimit([file(1000), file(501)], limits)).toMatch(/together/u);
  });
});

describe("a draft", () => {
  it("keeps the message on its way before what was written since, until the receipt", () => {
    expect(joined("first", "next")).toBe("first\n\nnext");
    expect(joined("", "next")).toBe("next");
    expect(joined("first", "")).toBe("first");
  });

  it("is cleared on the receipt of what was sent, and of nothing else", () => {
    expect(since("first", "first")).toBe("");
    expect(since("first\n\nnext\n\nmore", "first")).toBe("next\n\nmore");
    expect(since(joined("first", "first"), "first")).toBe("first");
  });

  it("of another tab is left whole by this tab's receipt", () => {
    // Tab A typed last, so the one stored draft is its text. Tab B's message is then received.
    expect(since("typed in tab A", "sent from tab B")).toBe("typed in tab A");
  });

  it("shows a message's line ends as the person typed them", () => {
    expect(plain("first\r\nsecond\r\n\r\nthird")).toBe("first\nsecond\n\nthird");
  });

  it("is left whole when it does not begin with what was sent", () => {
    expect(since("something else", "first")).toBe("something else");
    expect(since("firstly", "first")).toBe("firstly");
    expect(since("typed beside files", "")).toBe("typed beside files");
  });
});

describe("commands", () => {
  const commands = [
    { name: "skill:carillon-status" },
    { name: "skill:conversation-history" },
    { name: "compact" },
  ];

  it("offers the commands a slash and a prefix could become", () => {
    expect(offered("/", commands)).toEqual(commands);
    expect(offered("/skill:c", commands).map(({ name }) => name)).toEqual([
      "skill:carillon-status",
      "skill:conversation-history",
    ]);
    expect(offered("/compact keep", commands)).toEqual([]);
    expect(offered("a /", commands)).toEqual([]);
    expect(offered("", commands)).toEqual([]);
  });

  it("sends a text as a command only when it begins with a listed command's name", () => {
    expect(invokes("/compact", commands)).toBe(true);
    expect(invokes("/compact keep the decisions", commands)).toBe(true);
    expect(invokes("/compacting", commands)).toBe(false);
    expect(invokes("compact", commands)).toBe(false);
    expect(invokes("/usr/bin/env is missing", commands)).toBe(false);
  });
});

describe("a turn at work", () => {
  const now = 1134;

  it("does not say queued or running before the live read does", () => {
    const shown = ledger(turn("t1", "running"), undefined, undefined, now);
    expect(shown.phase).toBe("At work");
    expect(shown.waiting).toBe(false);
    expect(shown.controls).toEqual([]);
    expect(shown.tally).toBe("2 min 14 s");
  });

  it("waits when the live read says queued, whatever the turns say", () => {
    const shown = ledger(
      turn("t1", "running"),
      live("queued", { phase: "queued" }),
      undefined,
      now,
    );
    expect(shown.phase).toBe("Waiting its turn");
    expect(shown.waiting).toBe(true);
  });

  it("names each phase of a running turn", () => {
    const phases = /** @type {const} */ ([
      "starting",
      "model",
      "tools",
      "waiting",
      "retrying",
      "compacting",
    ]);
    expect(
      phases.map(
        (phase) => ledger(turn("t1", "queued"), live("running", { phase }), undefined, now).phase,
      ),
    ).toEqual([
      "Starting",
      "Waiting for the model",
      "Using tools",
      "Waiting",
      "Retrying the model request",
      "Compacting the conversation",
    ]);
    expect(ledger(turn("t1", "running"), live("running", null), undefined, now).phase).toBe(
      "At work",
    );
    expect(ledger(turn("t1", "running"), live("ended"), undefined, now).phase).toBe("Finishing");
  });

  it("draws one mark for each call: no longer listed, finished, failed and under way", () => {
    const shown = ledger(
      turn("t1", "running"),
      live("running", {
        tool_calls: 6,
        recent: [
          { tool: "grep", detail: '"beta" docs/', ok: true, duration_ms: 8 },
          { tool: "bash", ok: false, duration_ms: 41 },
          { tool: "read", ok: true },
        ],
        running: [{ tool: "bash", detail: "gh pr list", started_at: 1122, estimated: true }],
      }),
      undefined,
      now,
    );
    expect(shown.marks).toEqual(["past", "past", "done", "failed", "done", "running"]);
    expect(shown.counted).toBe("6 tool calls, 1 under way, 1 failed, 2 finished");
    expect(shown.calls).toEqual([
      { mark: "done", name: "grep", detail: '"beta" docs/', time: "8 ms" },
      { mark: "failed", name: "bash", detail: "", time: "failed, 41 ms" },
      { mark: "done", name: "read", detail: "", time: "" },
      { mark: "running", name: "bash", detail: "gh pr list", time: "about 12 s" },
    ]);
    expect(shown.tally).toBe("6 tool calls, 2 min 14 s");
  });

  it("keeps the row of marks bounded for a turn of very many calls", () => {
    const shown = ledger(
      turn("t1", "running"),
      live("running", { tool_calls: 400 }),
      undefined,
      now,
    );
    expect(shown.marks).toHaveLength(48);
    expect(shown.tally).toBe("400 tool calls, 2 min 14 s");
  });

  it("shows the model's commentary, or failing that its latest note", () => {
    const said = { headline: "Reading the notes.", notes: ["First.", "Second."] };
    expect(ledger(turn("t1", "running"), live("running", said), undefined, now).headline).toBe(
      "Reading the notes.",
    );
    expect(
      ledger(turn("t1", "running"), live("running", { notes: said.notes }), undefined, now)
        .headline,
    ).toBe("Second.");
    expect(ledger(turn("t1", "running"), live("running"), undefined, now).headline).toBe("");
  });

  it("lists delegated work with its state, and says what it could not show", () => {
    const shown = ledger(
      turn("t1", "running"),
      live("running", {
        partial: true,
        agent: { model: "gpt-6.1", effort: "high", context: { used: 20_000, window: 400_000 } },
        work: {
          partial: true,
          omitted: 3,
          items: [
            {
              id: "1",
              kind: "delegation",
              label: "Check the links",
              state: "running",
              actor: "scout",
              started_at: 1094,
            },
            {
              id: "2",
              kind: "task",
              label: "Draft notes",
              state: "succeeded",
              duration_ms: 90_000,
            },
            { id: "3", kind: "handoff", label: "To another model", state: "failed" },
            { id: "4", kind: "fork", label: "A branch", state: "queued" },
          ],
        },
      }),
      undefined,
      now,
    );
    expect(shown.work).toEqual([
      {
        mark: "running",
        name: "Delegated",
        detail: "Check the links (scout)",
        time: "running, 40 s",
      },
      { mark: "done", name: "Task", detail: "Draft notes", time: "done, 1 min 30 s" },
      { mark: "failed", name: "Handed off", detail: "To another model", time: "failed" },
      { mark: "waiting", name: "Fork", detail: "A branch", time: "queued" },
    ]);
    expect(shown.notes).toEqual([
      "3 more not shown.",
      "The runtime could not see all of the delegated work.",
      "Part of this report was left out for its size.",
      "gpt-6.1 high, context 5% full",
    ]);
  });

  it("offers only the controls the live read lists, in its order", () => {
    const offer = (/** @type {Live | undefined} */ read) =>
      ledger(turn("t1", "running"), read, undefined, now).controls.map(({ action }) => action);
    expect(offer(live("running", { controls: ["stop"] }))).toEqual(["stop"]);
    expect(offer(live("queued", { controls: ["steer", "interrupt", "stop"] }))).toEqual([
      "steer",
      "interrupt",
      "stop",
    ]);
    expect(offer(live("running"))).toEqual([]);
    expect(offer(live("running", null))).toEqual([]);
    expect(offer(undefined)).toEqual([]);
  });

  it("offers none once the turn has ended, and none for a turn the runtime opened", () => {
    expect(
      ledger(turn("t1", "running"), live("ended", { controls: ["stop"] }), undefined, now).controls,
    ).toEqual([]);
    expect(
      ledger(opened("t1"), live("running", { controls: ["stop"] }), undefined, now).controls,
    ).toEqual([]);
  });

  it("says what stop does to a waiting message and to a running answer", () => {
    const stop = (/** @type {Live["state"]} */ state) =>
      ledger(turn("t1", "running"), live(state, { controls: ["stop"] }), undefined, now)
        .controls[0];
    expect(stop("queued")).toEqual({
      action: "stop",
      label: "Stop",
      title: "Do not run this message.",
      asked: false,
    });
    expect(stop("running")?.title).toBe("End this answer.");
  });

  it("shows at once that a control was pressed, and offers no other meanwhile", () => {
    const shown = ledger(
      turn("t1", "running"),
      live("queued", { controls: ["interrupt", "stop"] }),
      "stop",
      now,
    );
    expect(shown.controls.map(({ label, asked }) => [label, asked])).toEqual([["Stopping", true]]);
  });

  it("keeps the pressed control on the turn when the live read stops listing it", () => {
    for (const read of [live("running"), live("running", null), live("running", { controls: [] })])
      expect(ledger(turn("t1", "running"), read, "stop", now).controls).toEqual([
        { action: "stop", label: "Stopping", title: "End this answer.", asked: true },
      ]);
  });

  it("says in words what the row of marks counts", () => {
    expect(ledger(turn("t1", "running"), live("running"), undefined, now).counted).toBe(
      "0 tool calls",
    );
    const one = { tool_calls: 1, running: [{ tool: "bash", started_at: 1130 }] };
    expect(ledger(turn("t1", "running"), live("running", one), undefined, now).counted).toBe(
      "1 tool call, 1 under way",
    );
  });
});

describe("a pressed control", () => {
  const asked = {
    action: /** @type {const} */ ("stop"),
    state: /** @type {const} */ ("running"),
    at: 100,
  };

  it("stays pressed while the turn is as it was", () => {
    expect(pressed(asked, live("running"), 101)).toBe("stop");
    expect(pressed(undefined, live("running"), 101)).toBeUndefined();
  });

  it("stays pressed for as long as the turn runs once the runtime stops listing it", () => {
    expect(pressed(asked, live("running"), 100 + 3600)).toBe("stop");
    expect(pressed(asked, live("running", null), 100 + 3600)).toBe("stop");
  });

  it("is released when the turn moves on, so an interrupted-in message can be stopped", () => {
    const interrupt = {
      action: /** @type {const} */ ("interrupt"),
      state: /** @type {const} */ ("queued"),
      at: 100,
    };
    expect(pressed(interrupt, live("queued"), 101)).toBe("interrupt");
    expect(pressed(interrupt, live("running"), 101)).toBeUndefined();
    expect(pressed(asked, live("ended"), 101)).toBeUndefined();
  });

  it("is released when the runtime still lists it after a quarter of a minute", () => {
    const listing = live("running", { controls: ["stop"] });
    expect(pressed(asked, listing, 115)).toBe("stop");
    expect(pressed(asked, listing, 116)).toBeUndefined();
  });
});

describe("an ended turn", () => {
  it("is described by its state and how long it took", () => {
    const report = {
      phase: /** @type {const} */ ("tools"),
      running: [],
      recent: [],
      tool_calls: 14,
    };
    expect(ending({ ...turn("a"), report })).toEqual({
      label: "Answered",
      took: "2 min 14 s, 14 tool calls",
      did: [],
    });
    expect(ending(turn("a", "stopped"))).toEqual({ label: "Stopped", took: "2 min 14 s", did: [] });
    expect(ending(turn("a", "interrupted"))?.label).toBe("Did not finish");
    expect(ending({ ...turn("a"), finished_at: 1000 })?.took).toBe("");
  });

  it("keeps the calls its final report lists", () => {
    const report = {
      phase: /** @type {const} */ ("model"),
      running: [],
      recent: [
        { tool: "bash", detail: "git log -5", ok: true, duration_ms: 41 },
        { tool: "read", ok: false },
      ],
      tool_calls: 12,
    };
    expect(ending({ ...turn("a"), report })?.did).toEqual([
      { mark: "done", name: "bash", detail: "git log -5", time: "41 ms" },
      { mark: "failed", name: "read", detail: "", time: "failed" },
    ]);
  });

  it("says of a steered message that its answer is below, on a later one", () => {
    // The runtime marks every message of a joined answer but the newest, so this is the earlier one.
    const steered = ending({ ...turn("a", "steered"), report: null });
    expect(steered).toEqual({
      label:
        "A later message joined this one. Their answer is below, on the newest message that took part.",
      took: "",
      did: [],
    });
  });

  it("is not described while it is unfinished", () => {
    expect(ending(turn("a", "running"))).toBeUndefined();
    expect(ending(turn("a", "queued"))).toBeUndefined();
  });
});

describe("numbers a person reads", () => {
  it("writes a length of time to two units", () => {
    expect([8, 999, 1000, 59_999, 134_000, 3_900_000].map(span)).toEqual([
      "8 ms",
      "999 ms",
      "1 s",
      "59 s",
      "2 min 14 s",
      "1 h 5 min",
    ]);
    expect([0, -5].map(span)).toEqual(["0 s", "0 s"]);
  });

  it("writes a file's size to two figures", () => {
    expect([0, 812, 3400, 20_417, 1_500_000, 1_073_741_824].map(size)).toEqual([
      "0 B",
      "812 B",
      "3.4 kB",
      "20 kB",
      "1.5 MB",
      "1.1 GB",
    ]);
  });

  it("says the time for today and the date before", () => {
    const at = (/** @type {string} */ iso) => new Date(iso).getTime() / 1000;
    const now = at("2026-11-12T15:00:00");
    expect(when(at("2026-11-12T14:02:00"), now, "en-GB")).toBe("14:02");
    expect(when(at("2026-11-03T14:02:00"), now, "en-GB")).toBe("3 Nov");
    expect(when(at("2025-11-03T14:02:00"), now, "en-GB")).toBe("3 Nov 2025");
  });
});
