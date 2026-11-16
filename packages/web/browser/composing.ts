import type { Locator, Page } from "@playwright/test";
import { message } from "./fixtures.ts";

/** A file a check makes up: its name and how many bytes it has. */
export type Chosen = { name: string; bytes: number };

/** The files that wait in the composer for the next message. */
export function attached(page: Page): Locator {
  return page.getByRole("list", { name: "Files to send" }).getByRole("listitem");
}

/** Chooses files as a person does, through the button and the browser's own chooser. */
export async function choose(page: Page, files: Chosen[]): Promise<void> {
  const chooser = page.waitForEvent("filechooser");
  await page.getByRole("button", { name: "Attach files" }).click();
  await (
    await chooser
  ).setFiles(
    files.map(({ name, bytes }) => ({
      name,
      mimeType: "application/octet-stream",
      buffer: Buffer.alloc(bytes, "x"),
    })),
  );
}

/** An expression the page evaluates to the files a person carries to it. */
function carried(files: Chosen[]): string {
  return `(() => {
    const transfer = new DataTransfer();
    for (const { name, bytes } of ${JSON.stringify(files)})
      transfer.items.add(new File(["x".repeat(bytes)], name));
    return transfer;
  })()`;
}

/** Drops files on the page. */
export async function drop(page: Page, files: Chosen[]): Promise<void> {
  const dataTransfer = await page.evaluateHandle(carried(files));
  await page.dispatchEvent("body", "drop", { dataTransfer });
}

/** Pastes files into the message, as a copied file or screenshot arrives. */
export async function paste(page: Page, files: Chosen[]): Promise<void> {
  await message(page).focus();
  await page.evaluate(
    `document.activeElement.dispatchEvent(new ClipboardEvent("paste", {
      clipboardData: ${carried(files)},
      bubbles: true,
      cancelable: true,
    }))`,
  );
}

function asked(page: Page, method: string, part: string): string[] {
  const urls: string[] = [];
  page.on("request", (request) => {
    if (request.method() === method && request.url().includes(part)) urls.push(request.url());
  });
  return urls;
}

/** The messages the page sends to the provider from now on, each attempt once. */
export function sends(page: Page): string[] {
  return asked(page, "PUT", "/messages/");
}

/** The times the page asks the provider for a conversation's commands from now on. */
export function commandReads(page: Page): string[] {
  return asked(page, "GET", "/commands");
}

/** The commands the composer offers for what is written. */
export function commands(page: Page): Locator {
  return page.getByRole("listbox", { name: "Commands" }).getByRole("option");
}
