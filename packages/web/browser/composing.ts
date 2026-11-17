import type { Locator, Page } from "@playwright/test";
import { message } from "./fixtures.ts";

export type Chosen = { name: string; bytes: number };

export function attached(page: Page): Locator {
  return page.getByRole("list", { name: "Files to send" }).getByRole("listitem");
}

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

function carried(files: Chosen[]): string {
  return `(() => {
    const transfer = new DataTransfer();
    for (const { name, bytes } of ${JSON.stringify(files)})
      transfer.items.add(new File(["x".repeat(bytes)], name));
    return transfer;
  })()`;
}

export async function drop(page: Page, files: Chosen[]): Promise<void> {
  const dataTransfer = await page.evaluateHandle(carried(files));
  await page.dispatchEvent("body", "drop", { dataTransfer });
}

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

export function sends(page: Page): string[] {
  return asked(page, "PUT", "/messages/");
}

export function commandReads(page: Page): string[] {
  return asked(page, "GET", "/commands");
}

export function commands(page: Page): Locator {
  return page.getByRole("listbox", { name: "Commands" }).getByRole("option");
}
