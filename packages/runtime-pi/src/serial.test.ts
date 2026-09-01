import { describe, expect, it } from "vitest";
import { serialize } from "./serial.ts";

describe("serialized runtime operations", () => {
  it("does not run an operation cancelled while queued", async () => {
    let releaseFirst = (): void => undefined;
    let markEntered = (): void => undefined;
    const entered = new Promise<void>((resolve) => {
      markEntered = resolve;
    });
    const gate = new Promise<void>((resolve) => {
      releaseFirst = resolve;
    });
    const first = serialize(async () => {
      markEntered();
      await gate;
    });
    await entered;

    const controller = new AbortController();
    let ran = false;
    const cancelled = serialize(() => {
      ran = true;
      return Promise.resolve();
    }, controller.signal);
    controller.abort();
    releaseFirst();

    await first;
    await expect(cancelled).rejects.toMatchObject({ name: "AbortError" });
    expect(ran).toBe(false);
  });
});
