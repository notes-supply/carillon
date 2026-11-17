import type { Page } from "@playwright/test";
import type { Fake } from "./fixtures.ts";

export async function tab(page: Page, state: "hidden" | "visible"): Promise<void> {
  await page.evaluate(
    `Object.defineProperty(document, "visibilityState", { value: "${state}", configurable: true });
    document.dispatchEvent(new Event("visibilitychange"));`,
  );
}

export async function streams(fake: Fake, count: number): Promise<void> {
  const path = `/fake/state?wait=streams&count=${String(count)}`;
  const answered = await fetch(fake.origin + path, { signal: AbortSignal.timeout(10_000) }).catch(
    () => undefined,
  );
  if (answered?.status !== 200)
    throw new Error(`the provider did not come to hold ${String(count)} streams`);
}

export async function streamsHeld(page: Page): Promise<unknown> {
  return page.evaluate(
    `fetch("/fake/state").then((answer) => answer.json()).then((state) => state.streams)`,
  );
}
