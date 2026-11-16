import { expect, test } from "vitest";
import { span, when } from "./words.ts";

test("a length of time is written to two units", () => {
  expect([8, 999, 1000, 59_999, 134_000, 3_900_000, 0, -5].map(span)).toStrictEqual([
    "8 ms",
    "999 ms",
    "1 s",
    "59 s",
    "2 min 14 s",
    "1 h 5 min",
    "0 s",
    "0 s",
  ]);
});

test("a moment is the time today and the date before", () => {
  const seconds = (iso: string) => new Date(iso).getTime() / 1000;
  const now = seconds("2026-11-12T15:00:00");
  expect(when(seconds("2026-11-12T14:02:00"), now, "en-GB")).toBe("14:02");
  expect(when(seconds("2026-11-03T14:02:00"), now, "en-GB")).toBe("3 Nov");
  expect(when(seconds("2025-11-03T14:02:00"), now, "en-GB")).toBe("3 Nov 2025");
});
