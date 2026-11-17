import type { Activity, FileLink, Turn } from "../src/api/stream.ts";
import {
  admit,
  audience,
  end,
  find,
  key,
  link,
  open,
  run,
  turnOf,
  working,
  world,
  type Recorded,
  type World,
} from "./world.ts";

export type Stage = {
  world: World;
  away(): void;
  returns(): void;
  signOut(): void;
  endStreams(): void;
  mute(): void;
  say(data: string): void;
  release(): void;
};

type Step = {
  atMs: number;
  label: string;
  after?: "send";
  run(stage: Stage): void;
};

export type Scenario = { about: string; world(unixSeconds: number): World; steps: Step[] };

type Pair = { conversation: Recorded; turn: Turn };

const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
  "base64",
);

function ago(held: World, seconds: number, seed: () => void): void {
  const now = held.unixSeconds;
  held.unixSeconds = now - seconds;
  seed();
  held.unixSeconds = now;
}

function message(held: World, n: number, text: string, files: FileLink[] = []): Pair {
  const eventId = `m-${audience(100_000 + held.turnByEvent.size)}`;
  admit(held, key(n), eventId, text, files);
  const pair = turnOf(held, eventId);
  if (pair === undefined) throw new Error("a seeded message has a turn");
  return pair;
}

function exchange(
  held: World,
  n: number,
  seconds: number,
  took: number,
  text: string,
  html: string,
  report?: Activity,
): void {
  let pair: Pair | undefined;
  ago(held, seconds, () => {
    pair = message(held, n, text);
  });
  ago(held, seconds - took, () => {
    if (pair !== undefined)
      end(held, pair.conversation, pair.turn, { kind: "answered", html, files: [] }, report);
  });
}

const IDLE: Activity = { phase: "model", running: [], recent: [], tool_calls: 0 };

const ANSWER = `<h2>Release plan</h2>
<p>The notes are <strong>ready</strong>. Two things changed since <a target="_blank" rel="noreferrer noopener" href="https://example.com/changelog">the last release</a>:</p>
<ul>
<li>Core pushes complete snapshots.</li>
<li>The page replaces its state with each one.</li>
</ul>
<table><thead><tr><th class="align-left">Day</th><th class="align-right">Turns</th></tr></thead><tbody>
<tr><td class="align-left">Monday</td><td class="align-right">58</td></tr>
</tbody></table>
<pre><code class="language-sh">carillon restart
</code></pre>
<p>&lt;script&gt;alert(1)&lt;/script&gt; is shown as text.</p>
`;

const REPORT: Activity = {
  phase: "model",
  running: [],
  recent: [
    { tool: "read", detail: "docs/RELEASE.md", ok: true, duration_ms: 3200 },
    { tool: "bash", detail: "cargo test", ok: false },
    { tool: "bash", detail: "git log -5", ok: true, duration_ms: 41 },
    { tool: "write", detail: "notes.txt", ok: true, duration_ms: 12 },
  ],
  tool_calls: 4,
  agent: { model: "gpt-6.1", effort: "high", context: { used: 18_250, window: 400_000 } },
};

function planning(now: number): World {
  const held = world(now);
  exchange(
    held,
    1,
    600,
    8,
    "Plan the release.\nStart with the notes.",
    "<p>I will start with the notes.</p>\n",
  );
  return held;
}

const settling: Step[] = [
  {
    atMs: 1000,
    label: "core hands the turn to its runtime",
    after: "send",
    run: ({ world: held }) => {
      run(working(held).turn, { phase: "starting", running: [], recent: [], tool_calls: 0 }, [
        "stop",
      ]);
    },
  },
  {
    atMs: 3000,
    label: "the model is asked",
    run: ({ world: held }) => {
      const activity: Activity = { ...IDLE, headline: "Reading the release notes." };
      run(working(held).turn, activity, ["stop"]);
    },
  },
  {
    atMs: 6000,
    label: "a tool call is under way",
    run: ({ world: held }) => {
      const activity: Activity = {
        phase: "tools",
        headline: "Reading the release notes.",
        running: [{ tool: "read", detail: "docs/RELEASE.md", started_at: held.unixSeconds }],
        recent: [],
        tool_calls: 1,
      };
      run(working(held).turn, activity, ["stop"]);
    },
  },
  {
    atMs: 10_000,
    label: "one call is done, one failed and one is under way",
    run: ({ world: held }) => {
      const activity: Activity = {
        phase: "tools",
        headline: "Checking the tests before the notes.",
        running: [
          { tool: "bash", detail: "git log -5", started_at: held.unixSeconds - 1, estimated: true },
        ],
        recent: [
          { tool: "read", detail: "docs/RELEASE.md", ok: true, duration_ms: 3200 },
          { tool: "bash", detail: "cargo test", ok: false },
        ],
        tool_calls: 3,
      };
      run(working(held).turn, activity, ["stop"]);
    },
  },
  {
    atMs: 15_000,
    label: "work is delegated, with notes",
    run: ({ world: held }) => {
      const activity: Activity = {
        phase: "waiting",
        headline: "Comparing the changelog with the commits.",
        running: [],
        recent: [
          { tool: "read", detail: "docs/RELEASE.md", ok: true, duration_ms: 3200 },
          { tool: "bash", detail: "cargo test", ok: false },
          { tool: "bash", detail: "git log -5", ok: true, duration_ms: 41 },
        ],
        tool_calls: 3,
        work: {
          items: [
            {
              id: "w-1",
              kind: "delegation",
              label: "Check the changelog",
              state: "running",
              actor: "scout",
              started_at: held.unixSeconds - 2,
            },
            { id: "w-2", kind: "task", label: "Draft the summary", state: "queued" },
          ],
          partial: false,
          omitted: 2,
        },
        notes: ["The changelog is out of date."],
        agent: { model: "gpt-6.1", effort: "high", context: { used: 18_250, window: 400_000 } },
      };
      run(working(held).turn, activity, ["stop"]);
    },
  },
  {
    atMs: 22_000,
    label: "the turn ends with an answer and two files",
    run: ({ world: held }) => {
      const { conversation, turn } = working(held);
      const files = [
        link(held, "chart.png", "image/png", PNG),
        link(held, "notes.txt", "application/octet-stream", Buffer.from("Release notes.\n")),
      ];
      end(held, conversation, turn, { kind: "answered", html: ANSWER, files }, REPORT);
    },
  },
];

function rail(now: number): World {
  const held = world(now);
  exchange(
    held,
    4,
    3 * 86_400,
    30,
    "Summarise the standup notes.",
    "<p>Three people are blocked on the release.</p>\n",
  );
  ago(held, 86_400 + 3600, () => {
    const numbers = link(
      held,
      "numbers.csv",
      "application/octet-stream",
      Buffer.from("day,turns\n"),
    );
    const pair = message(held, 3, "", [numbers]);
    const html = "<p>I have the file.</p>\n";
    end(held, pair.conversation, pair.turn, { kind: "answered", html, files: [] });
  });
  exchange(
    held,
    1,
    1200,
    8,
    "Plan the release.\nStart with the notes.",
    "<p>I will start with the notes.</p>\n",
  );
  exchange(held, 2, 900, 20, "Chart last week's numbers.", "<p>Which chart do you want?</p>\n");
  ago(held, 300, () => {
    const pair = message(held, 2, "A bar chart, one bar a day.");
    const drawing: Activity = {
      ...IDLE,
      phase: "tools",
      headline: "Drawing the chart.",
      tool_calls: 2,
    };
    run(pair.turn, drawing, ["stop"]);
  });
  return held;
}

function header(now: number): World {
  const held = rail(now);
  held.agents.push({ id: "scout", name: "Scout" });
  held.custom_site = true;
  ago(held, 120, () => {
    const scouting = { agent_id: "scout", audience_id: audience(5) };
    const eventId = `m-${audience(100_000 + held.turnByEvent.size)}`;
    admit(held, scouting, eventId, "Scout the changelog.", []);
    const pair = turnOf(held, eventId);
    if (pair === undefined) throw new Error("a seeded message has a turn");
    const html = "<p>The changelog is two releases behind.</p>\n";
    end(held, pair.conversation, pair.turn, { kind: "answered", html, files: [] });
  });
  return held;
}

function pictured(now: number): World {
  const held = world(now);
  ago(held, 600, () => {
    const pair = message(held, 1, "Chart last week's numbers.", [
      link(held, "sketch.png", "image/png", PNG),
    ]);
    const files = [
      link(held, "chart.png", "image/png", PNG),
      link(held, "numbers.csv", "application/octet-stream", Buffer.from("day,turns\n")),
    ];
    end(held, pair.conversation, pair.turn, {
      kind: "answered",
      html: "<p>Here is the chart.</p>\n",
      files,
    });
  });
  return held;
}

function renewLinks(held: World): void {
  const expires = (Math.floor(held.unixSeconds / 86_400) + 2) * 86_400;
  const renewed = (file: FileLink): FileLink => ({
    ...file,
    url: file.url.replace(/exp=\d+/u, `exp=${String(expires)}`),
  });
  for (const { turns } of held.conversations)
    for (const turn of turns) {
      if (turn.source.kind === "message") turn.source.files = turn.source.files.map(renewed);
      if (turn.progress.state === "ended" && turn.progress.ending.kind === "answered")
        turn.progress.ending.files = turn.progress.ending.files.map(renewed);
    }
}

const railSteps: Step[] = [
  {
    atMs: 5000,
    label: "the runtime answers a turn of its own in the standup conversation",
    run: ({ world: held }) => {
      const conversation = find(held, key(4));
      if (conversation === undefined) throw new Error("the rail has the standup conversation");
      const turn = open(held, conversation, "Scheduled run: standup follow-up");
      end(held, conversation, turn, {
        kind: "answered",
        html: "<p>Two are unblocked now.</p>\n",
        files: [],
      });
    },
  },
  {
    atMs: 9000,
    label: "the chart conversation's turn is answered",
    run: ({ world: held }) => {
      const { conversation, turn } = working(held);
      end(held, conversation, turn, {
        kind: "answered",
        html: "<p>Here is the chart.</p>\n",
        files: [],
      });
    },
  },
];

function endings(now: number): World {
  const held = world(now);
  const interrupted =
    "<p>Carillon restarted before this turn finished. Send the message again.</p>\n";
  ago(held, 900, () => {
    const pair = message(held, 1, "Summarise the meeting.");
    held.unixSeconds += 4;
    end(held, pair.conversation, pair.turn, { kind: "interrupted", html: interrupted });
  });
  let steered: Pair | undefined;
  ago(held, 800, () => {
    steered = message(held, 1, "Start the long report.");
  });
  ago(held, 780, () => {
    const pair = message(held, 1, "Also include last week's figures.");
    if (steered !== undefined) end(held, steered.conversation, steered.turn, { kind: "steered" });
    held.unixSeconds += 95;
    end(
      held,
      pair.conversation,
      pair.turn,
      { kind: "answered", html: "<p>The report has both weeks.</p>\n", files: [] },
      REPORT,
    );
  });
  ago(held, 600, () => {
    const pair = message(held, 1, "Run the full benchmark.");
    held.unixSeconds += 12;
    end(held, pair.conversation, pair.turn, { kind: "stopped" });
  });
  ago(held, 500, () => {
    const conversation = find(held, key(1));
    if (conversation === undefined) throw new Error("the conversation was seeded");
    const labelled = open(held, conversation, "Scheduled run: morning summary");
    held.unixSeconds += 6;
    end(held, conversation, labelled, {
      kind: "answered",
      html: "<p>Nothing new this morning.</p>\n",
      files: [],
    });
    const bare = open(held, conversation, undefined);
    held.unixSeconds += 3;
    end(held, conversation, bare, {
      kind: "answered",
      html: "<p>A subagent reported back.</p>\n",
      files: [],
    });
  });
  ago(held, 60, () => {
    run(message(held, 1, "And now?").turn, null, []);
  });
  ago(held, 30, () => {
    message(held, 1, "One more thing.");
  });
  return held;
}

function controls(now: number): World {
  const held = world(now);
  ago(held, 120, () => {
    const activity: Activity = {
      ...IDLE,
      phase: "tools",
      headline: "Writing the report.",
      tool_calls: 5,
    };
    run(message(held, 1, "Write the long report.").turn, activity, ["stop"]);
  });
  ago(held, 20, () => {
    const waiting: Activity = { phase: "queued", running: [], recent: [], tool_calls: 0 };
    run(message(held, 1, "Use last week's figures instead.").turn, waiting, [
      "steer",
      "interrupt",
      "stop",
    ]);
  });
  return held;
}

function act({ world: held }: Stage): void {
  const pressed = held.pressed;
  if (pressed === undefined) throw new Error("no control was pressed before this step");
  const conversation = find(held, key(1));
  const [answering, waiting] = conversation?.turns ?? [];
  const target = turnOf(held, pressed.event_id)?.turn;
  if (conversation === undefined || answering === undefined || waiting === undefined)
    throw new Error("the controls scenario has two turns");
  const going: Activity = {
    ...IDLE,
    phase: "tools",
    headline: "Writing the report.",
    tool_calls: 6,
  };
  if (target === answering) {
    end(held, conversation, answering, { kind: "stopped" });
    run(waiting, { ...IDLE, headline: "Starting on last week's figures." }, ["stop"]);
  } else if (pressed.action === "stop") {
    end(held, conversation, waiting, { kind: "stopped" });
  } else {
    end(held, conversation, answering, {
      kind: pressed.action === "steer" ? "steered" : "stopped",
    });
    run(waiting, going, ["stop"]);
  }
  held.pressed = undefined;
}

function many(now: number, turns: number, html: (n: number) => string): World {
  const held = world(now);
  for (let n = 1; n <= turns; n += 1)
    exchange(held, 1, (turns - n + 1) * 120, 30, `Message ${String(n)}`, html(n));
  return held;
}

function crowded(now: number): World {
  const held = world(now);
  for (let n = 1; n <= 60; n += 1)
    exchange(
      held,
      n,
      (61 - n) * 600,
      30,
      `Conversation ${String(n)}`,
      `<p>Answer ${String(n)}.</p>\n`,
    );
  return held;
}

const TALL = Array.from(
  { length: 12 },
  (_, line) =>
    `<p>Paragraph ${String(line + 1)} of a long answer, long enough that one turn is taller than the window it is read in.</p>`,
).join("\n");

function large(now: number, window: World["window"]): World {
  const held = many(
    now,
    499,
    (n) =>
      `<p>Answer ${String(n)} has a list and some code.</p>\n<ul>\n<li>One</li>\n<li>Two</li>\n</ul>\n<pre><code>step ${String(n)}\n</code></pre>\n`,
  );
  held.window = window;
  ago(held, 5, () => {
    run(message(held, 1, "Message 500").turn, { ...IDLE, headline: "Tick 0" }, ["stop"]);
  });
  return held;
}

const ticks: Step[] = Array.from({ length: 240 }, (_, index) => ({
  atMs: (index + 1) * 1000,
  label: `tick ${String(index + 1)}`,
  run: ({ world: held }: Stage) => {
    const activity: Activity = {
      phase: "tools",
      headline: `Tick ${String(index + 1)}`,
      running: [],
      recent: [],
      tool_calls: index + 1,
    };
    run(working(held).turn, activity, ["stop"]);
  },
}));

function answers(label: string, html: string): Step["run"] {
  return ({ world: held }) => {
    const conversation = find(held, key(1));
    if (conversation === undefined) throw new Error("the scenario has its first conversation");
    const turn = open(held, conversation, label);
    end(held, conversation, turn, { kind: "answered", html, files: [] });
  };
}

export const scenarios: Record<string, Scenario> = {
  empty: { about: "An installation with no conversation yet.", world, steps: [] },
  rail: {
    about:
      "Four conversations: one at work, one of files only. An answer then lands in one that is not open.",
    world: rail,
    steps: railSteps,
  },
  door: {
    about: "The rail's world, for signing in with a wrong password and then the right one.",
    world: rail,
    steps: [],
  },
  header: {
    about:
      "The rail's world with two agents, each with a conversation, served from the owner's own copy of the page.",
    world: header,
    steps: [],
  },
  turn: {
    about:
      "A message from its send to its answer: queued, running through every kind of activity, answered with Markdown and files.",
    world: planning,
    steps: settling,
  },
  "first-message": {
    about: "A conversation core has not recorded becomes recorded with its first message.",
    world,
    steps: settling,
  },
  agents: {
    about:
      "An installation with two agents and no conversation yet, for choosing who a new one is with.",
    world: (now) => ({
      ...world(now),
      agents: [
        { id: "assistant", name: "Carillon" },
        { id: "researcher", name: "Research" },
      ],
    }),
    steps: [],
  },
  endings: {
    about:
      "One conversation with every ending: interrupted, steered, stopped, a runtime-opened turn, a running turn with no activity and a queued one.",
    world: endings,
    steps: [
      {
        atMs: 4000,
        label: "the running turn reports its activity",
        run: ({ world: held }) => {
          const conversation = find(held, key(1));
          const turn = conversation?.turns.find((each) => each.progress.state === "running");
          if (turn === undefined) throw new Error("the endings scenario has a running turn");
          run(turn, { ...IDLE, headline: "Thinking about what is next." }, ["stop"]);
        },
      },
    ],
  },
  "send-refused": {
    about: "The provider refuses a message as too large, then takes messages again.",
    world: (now) => ({
      ...planning(now),
      sending: {
        kind: "refuse",
        status: 413,
        error: "A file may have at most 1024 MiB, and a message's files 2048 MiB together.",
      },
    }),
    steps: [
      {
        atMs: 5000,
        label: "the provider takes messages again",
        run: ({ world: held }) => {
          held.sending = { kind: "accept" };
        },
      },
    ],
  },
  limits: {
    about:
      "A provider that takes three files of 2 kB each and 3 kB together, so that a few bytes are too many.",
    world: (now) => ({
      ...planning(now),
      limits: { files: 3, file_bytes: 2000, message_bytes: 3000 },
    }),
    steps: [],
  },
  "send-lost-receipt": {
    about:
      "Core takes a message and goes away before its receipt arrives. It returns, and the same event ID is answered as a duplicate.",
    world: (now) => ({ ...planning(now), sending: { kind: "lose_receipt" } }),
    steps: [
      {
        atMs: 8000,
        label: "core returns",
        run: (stage) => {
          stage.world.sending = { kind: "accept" };
          stage.returns();
        },
      },
    ],
  },
  "send-held": {
    about:
      "The provider holds a message, reading none of its upload, until the next step. Until then the page shows it as pending.",
    world: (now) => ({ ...planning(now), sending: { kind: "hold_upload" } }),
    steps: [
      {
        atMs: 6000,
        label: "the provider reads the upload",
        run: (stage) => {
          stage.world.sending = { kind: "accept" };
          stage.release();
        },
      },
    ],
  },
  "send-held-refused": {
    about:
      "The provider holds a message, then reads it and refuses it, so that a person has written more by then.",
    world: (now) => ({ ...planning(now), sending: { kind: "hold_upload" } }),
    steps: [
      {
        atMs: 6000,
        label: "the provider reads the upload and refuses it",
        run: (stage) => {
          stage.world.sending = {
            kind: "refuse",
            status: 413,
            error: "A file may have at most 1024 MiB, and a message's files 2048 MiB together.",
          };
          stage.release();
        },
      },
    ],
  },
  "send-signed-out": {
    about:
      "The password changes while the provider holds a message. The provider then reads what it was sent.",
    world: (now) => ({ ...planning(now), sending: { kind: "hold_upload" } }),
    steps: [
      { atMs: 2000, label: "the password changes", run: (stage) => stage.signOut() },
      {
        atMs: 6000,
        label: "the provider reads the upload",
        run: (stage) => {
          stage.world.sending = { kind: "accept" };
          stage.release();
        },
      },
    ],
  },
  controls: {
    about:
      "A running answer that offers stop, and a waiting message that offers steer, interrupt and stop. The next step does what was pressed.",
    world: controls,
    steps: [
      { atMs: 3000, label: "the runtime acts on the control", run: act },
      {
        atMs: 9000,
        label: "the turn that goes on is answered",
        run: ({ world: held }) => {
          const { conversation, turn } = working(held);
          end(held, conversation, turn, {
            kind: "answered",
            html: "<p>The report is done.</p>\n",
            files: [],
          });
        },
      },
    ],
  },
  commands: {
    about: "A runtime with three commands. A message that begins with one is marked as a command.",
    world: (now) => ({
      ...planning(now),
      commands: [
        { name: "status", description: "Report Carillon's status." },
        { name: "skill:carillon-status", description: "Report a job's status.", hint: "[job]" },
        { name: "compact" },
      ],
    }),
    steps: [
      {
        atMs: 2000,
        label: "the command is answered",
        after: "send",
        run: ({ world: held }) => {
          const { conversation, turn } = working(held);
          end(held, conversation, turn, {
            kind: "answered",
            html: "<p>Carillon is running.</p>\n",
            files: [],
          });
        },
      },
    ],
  },
  unavailable: {
    about: "Core goes away while a turn runs. It returns having ended that turn.",
    world: (now) => {
      const held = planning(now);
      ago(held, 30, () => {
        run(message(held, 1, "Draft the notes.").turn, { ...IDLE, headline: "Drafting." }, [
          "stop",
        ]);
      });
      return held;
    },
    steps: [
      { atMs: 2000, label: "core goes away", run: (stage) => stage.away() },
      {
        atMs: 9000,
        label: "core returns, and the turn it held did not finish",
        run: (stage) => {
          const { conversation, turn } = working(stage.world);
          const html =
            "<p>Carillon restarted before this turn finished. Send the message again.</p>\n";
          end(stage.world, conversation, turn, { kind: "interrupted", html });
          stage.returns();
        },
      },
    ],
  },
  "stream-ends": {
    about: "The provider ends the stream without a word, and an answer lands after it.",
    world: planning,
    steps: [
      { atMs: 2000, label: "the provider ends the stream", run: (stage) => stage.endStreams() },
      {
        atMs: 4000,
        label: "the runtime answers a turn of its own",
        run: answers("Scheduled run: release check", "<p>The release is on schedule.</p>\n"),
      },
    ],
  },
  "stream-silent": {
    about: "The provider's stream stays open and says nothing more, and an answer lands after it.",
    world: planning,
    steps: [
      { atMs: 2000, label: "the provider's stream goes silent", run: (stage) => stage.mute() },
      {
        atMs: 4000,
        label: "the runtime answers a turn of its own",
        run: answers("Scheduled run: release check", "<p>The release is on schedule.</p>\n"),
      },
    ],
  },
  pictures: {
    about:
      "A message and its answer, with pictures and a file. The day ends: the provider ends the stream, and the next has links for another day.",
    world: pictured,
    steps: [
      {
        atMs: 86_400_000,
        label: "the day ends, and the provider ends the stream",
        run: (stage) => {
          renewLinks(stage.world);
          stage.endStreams();
        },
      },
    ],
  },
  "signed-out": {
    about: "The password changes while the page is open.",
    world: planning,
    steps: [{ atMs: 2000, label: "the password changes", run: (stage) => stage.signOut() }],
  },
  "signed-out-quietly": {
    about:
      "The password changes and the open stream is not told, as for up to ten seconds it may not be. Every request then is refused as signed out.",
    world: controls,
    steps: [
      {
        atMs: 2000,
        label: "the password changes",
        run: ({ world: held }) => {
          held.passwordVersion += 1;
        },
      },
    ],
  },
  unreadable: {
    about: "The provider sends a message this page's contract does not have, then a snapshot.",
    world: planning,
    steps: [
      {
        atMs: 2000,
        label: "the provider sends what the page cannot read",
        run: (stage) => stage.say('{"kind":"snapshot","snapshot":{"revision":2}}'),
      },
      {
        atMs: 4000,
        label: "the runtime answers a turn of its own",
        run: answers("Scheduled run: release check", "<p>The release is on schedule.</p>\n"),
      },
    ],
  },
  crowded: {
    about: "Sixty conversations, more than one window of the rail.",
    world: crowded,
    steps: [],
  },
  earlier: {
    about: "One conversation of 120 turns, more than two windows.",
    world: (now) => many(now, 120, (n) => `<p>Answer ${String(n)}.</p>\n`),
    steps: [],
  },
  long: {
    about:
      "Thirty turns, each taller than a window. Turns the runtime opens then arrive at its end.",
    world: (now) => many(now, 30, (n) => `<p>Answer ${String(n)}.</p>\n${TALL}\n`),
    steps: [
      {
        atMs: 2000,
        label: "a first turn arrives",
        run: answers("Scheduled run: first", "<p>First arrival.</p>\n"),
      },
      {
        atMs: 4000,
        label: "a second turn arrives",
        run: answers("Scheduled run: second", "<p>Second arrival.</p>\n"),
      },
    ],
  },
  large: {
    about:
      "One conversation of 500 turns, the newest running. Each of 240 steps changes that one turn.",
    world: (now) => large(now, "kept"),
    steps: ticks,
  },
  "large-all": {
    about:
      "The large conversation, sent whole whatever window the view asks for: a measuring device, not what a provider does.",
    world: (now) => large(now, "ignored"),
    steps: ticks,
  },
};
