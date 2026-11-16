import { expect, test } from "vitest";
import type { Activity } from "../api/stream.ts";
import { outcome } from "./outcome.ts";

const ACCEPTED = 1000;
const IDLE: Activity = { phase: "model", running: [], recent: [], tool_calls: 0 };

test("an ended turn says how it ended, how long it took and what it did", () => {
  const report: Activity = {
    ...IDLE,
    tool_calls: 4,
    recent: [{ tool: "read", detail: "docs/RELEASE.md", ok: true, duration_ms: 3200 }],
  };
  const answered = { kind: "answered", html: "<p>Done.</p>\n", files: [] } as const;
  expect(
    outcome(
      { state: "ended", finished_at: 1021, ending: { ...answered, files: [] }, report },
      ACCEPTED,
    ),
  ).toStrictEqual({
    label: "Answered",
    took: "21 s, 4 tool calls",
    did: [{ mark: "done", name: "read", detail: "docs/RELEASE.md", time: "3 s" }],
    unlisted: true,
  });
  expect(
    outcome({ state: "ended", finished_at: 1134, ending: { kind: "stopped" } }, ACCEPTED),
  ).toStrictEqual({
    label: "Stopped",
    took: "2 min 14 s",
    did: [],
    unlisted: false,
  });
  const interrupted = { kind: "interrupted", html: "<p>Core restarted.</p>\n" } as const;
  expect(outcome({ state: "ended", finished_at: 1000, ending: interrupted }, ACCEPTED).label).toBe(
    "Did not finish",
  );
  expect(outcome({ state: "ended", finished_at: 1000, ending: interrupted }, ACCEPTED).took).toBe(
    "",
  );
});

test("a steered turn points at the answer below it and claims no time of its own", () => {
  const steered = outcome(
    {
      state: "ended",
      finished_at: 1134,
      ending: { kind: "steered" },
      report: { ...IDLE, tool_calls: 3 },
    },
    ACCEPTED,
  );
  expect(steered.took).toBe("");
  expect(steered.label).toMatch(/^A later message joined this one\./u);
});
