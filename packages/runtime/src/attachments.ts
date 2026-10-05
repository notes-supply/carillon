/**
 * A turn's captured files as every runtime reads them: by their path below
 * the installation's attachment directory, and, for images, by the format
 * their own header declares rather than the sender's label.
 */
import type { TurnAttachment } from "@carillon/protocol";
import { imageSize } from "image-size";
import { open } from "node:fs/promises";
import { resolve, sep } from "node:path";

/** Enough of any supported image to read its header. */
export const HEADER_BYTES = 512 * 1024;

/** The image formats runtimes pass as images, by `image-size` type. */
const MEDIA_TYPES: Readonly<Record<string, string>> = {
  png: "image/png",
  jpg: "image/jpeg",
  webp: "image/webp",
  gif: "image/gif",
};

export interface DetectedImage {
  readonly mediaType: string;
  readonly width: number;
  readonly height: number;
}

/** A captured file's location, refusing anything that is not below the root. */
export function capturedPath(root: string, attachment: TurnAttachment): string {
  const base = resolve(root);
  const path = resolve(base, attachment.path);
  if (!path.startsWith(base + sep)) throw new Error("attachment path escapes the attachment root");
  return path;
}

/**
 * A PNG, JPEG, WebP or GIF image's media type and declared dimensions, from
 * its header without decoding it. Other formats, including ones image-size
 * recognizes only loosely, are files.
 */
export function detectImage(header: Uint8Array): DetectedImage | undefined {
  try {
    const { type, width, height } = imageSize(header);
    const mediaType = type === undefined ? undefined : MEDIA_TYPES[type];
    return mediaType === undefined ? undefined : { mediaType, width, height };
  } catch {
    return undefined;
  }
}

/** Up to `length` bytes from the start of the file; undefined when it cannot be opened. */
export async function readPrefix(path: string, length: number): Promise<Uint8Array | undefined> {
  const file = await open(path, "r").catch(() => undefined);
  if (file === undefined) return undefined;
  try {
    const buffer = new Uint8Array(length);
    let filled = 0;
    while (filled < length) {
      const { bytesRead } = await file.read(buffer, filled, length - filled, filled);
      if (bytesRead === 0) break;
      filled += bytesRead;
    }
    return buffer.subarray(0, filled);
  } finally {
    await file.close();
  }
}
