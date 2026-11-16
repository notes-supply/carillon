import { expect, test as base, type Locator, type Page } from "@playwright/test";
import { start } from "../fake/start.ts";
import { EPOCH, PASSWORD } from "../fake/world.ts";

export { EPOCH, PASSWORD };

/** The fake provider one worker runs, serving the built page. */
export type Fake = {
  origin: string;
  /** Starts a scenario over. The fake then moves only when `step` is called. */
  play(scenario: string): Promise<void>;
  /** Plays the scenario's next step, which must be the one named. */
  step(label: string): Promise<void>;
};

/** What `playwright.config.ts` sets for every check. */
export type Options = {
  /** The folder of the built page. */
  site: string;
};

type Worker = { origin: string };

async function post(origin: string, path: string, body: object): Promise<void> {
  const response = await fetch(origin + path, { method: "POST", body: JSON.stringify(body) });
  if (response.status !== 200) throw new Error(`${path}: ${await response.text()}`);
}

export const test = base.extend<{ fake: Fake; guard: undefined }, Options & { provider: Worker }>({
  site: ["", { option: true, scope: "worker" }],
  provider: [
    async ({ site }, use) => {
      const fake = await start("empty", site);
      await use({ origin: fake.origin });
      fake.stop();
    },
    { scope: "worker" },
  ],
  fake: async ({ provider }, use) => {
    await use({
      origin: provider.origin,
      play: (scenario) =>
        post(provider.origin, "/fake/scenario", { name: scenario, clock: "manual" }),
      step: (label) => post(provider.origin, "/fake/step", { expect: label }),
    });
  },
  /** Every check also fails when the page breaks its policy, throws, or opens a dialog. */
  guard: [
    async ({ page }, use) => {
      const faults: string[] = [];
      page.on("pageerror", (error) => faults.push(`the page threw: ${error.message}`));
      page.on("dialog", (dialog) => {
        faults.push(`the page opened a dialog: ${dialog.message()}`);
        void dialog.dismiss();
      });
      page.on("console", (message) => {
        if (message.text().includes("Content Security Policy")) faults.push(message.text());
      });
      await use(undefined);
      expect(faults).toEqual([]);
    },
    { auto: true },
  ],
});

export { expect };

/** The page's clock reads `seconds` after the moment every scenario begins. Timers still run. */
export async function clockAt(page: Page, seconds: number): Promise<void> {
  await page.clock.setFixedTime(new Date((EPOCH + seconds) * 1000));
}

/** Opens the page at a scenario's beginning and signs in as a person does. */
export async function arrive(page: Page, fake: Fake, scenario: string): Promise<void> {
  await fake.play(scenario);
  await clockAt(page, 0);
  await page.goto(fake.origin);
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(rail(page)).toBeVisible();
}

export function rail(page: Page): Locator {
  return page.getByRole("navigation", { name: "Conversations" });
}

/** The conversations listed, in the order shown. */
export function conversations(page: Page): Locator {
  return rail(page).getByRole("list").first().locator("> li");
}

export function conversation(page: Page, title: string): Locator {
  return conversations(page).filter({ hasText: title });
}

/** Opens a listed conversation and waits until the room is its. */
export async function open(page: Page, title: string): Promise<void> {
  await conversation(page, title).getByRole("link").click();
  await expect(page.getByRole("heading", { name: title, level: 2 })).toBeVisible();
}

/** The open conversation's turns, oldest first, with any message still on its way. */
export function turns(page: Page): Locator {
  return page.getByRole("list", { name: "Turns" }).locator("> li");
}

export function message(page: Page): Locator {
  return page.getByRole("textbox", { name: "Message" });
}

export async function send(page: Page, text: string): Promise<void> {
  await message(page).fill(text);
  await message(page).press("Enter");
}

/** What a turn at work shows of its runtime. */
export function atWork(turn: Locator): Locator {
  return turn.getByRole("region", { name: "This turn is at work" });
}
