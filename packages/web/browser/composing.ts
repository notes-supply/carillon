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

/** The messages the page has sent to the provider, each attempt once. */
export function sends(page: Page): string[] {
  const urls: string[] = [];
  page.on("request", (request) => {
    if (request.method() === "PUT" && request.url().includes("/messages/"))
      urls.push(request.url());
  });
  return urls;
}
