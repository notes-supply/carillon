/**
 * A turn's images as T3 input. Each PNG, JPEG, WebP or GIF file the turn
 * carries, by its own header and within T3's limits, is uploaded to T3 the
 * way T3's own clients upload one (`attachments.createUploadUrl`, then the
 * bytes to the signed URL it returns) and referenced by the turn's message;
 * every other file is named to the model as unavailable. Nothing is
 * recorded: an upload the message never references is swept by T3.
 */
import { createHash } from "node:crypto";
import type { TurnAttachment } from "@carillon/protocol";
import { capturedPath, detectImage, HEADER_BYTES, readPrefix } from "@carillon/runtime/attachments";
import { ExplainedFailure } from "@carillon/runtime/failure";
import { Type } from "typebox";
import { Check } from "typebox/value";
import { RPC, type NativeImage } from "./contracts.ts";
import { T3Failure, type T3Connection, type T3Endpoint } from "./t3.ts";

/** T3's limits for a message's images (`chatAttachment.ts`). */
const MAX_IMAGE_BYTES = 10 * 1024 * 1024;
const MAX_TOTAL_BYTES = 80 * 1024 * 1024;
/** T3's limit for an attachment's name, in UTF-16 code units. */
const NAME_LIMIT = 255;

const UploadUrlSchema = Type.Object({
  attachmentId: Type.String({ minLength: 1 }),
  relativeUrl: Type.String({ minLength: 1 }),
});

export interface PreparedImages {
  /** The uploaded images, in message order. */
  readonly images: readonly NativeImage[];
  /** Names of the files T3 does not receive. */
  readonly unavailable: readonly string[];
}

/**
 * Uploads the turn's images, each as the bytes core captured, by their
 * recorded digest. A file that changed since core captured it, or an upload
 * T3 refuses, fails the turn before anything is dispatched; a lost
 * connection is a `T3Failure`, so the caller may start again.
 */
export async function uploadImages(
  connection: T3Connection,
  endpoint: T3Endpoint,
  root: string,
  attachments: readonly TurnAttachment[],
  signal: AbortSignal,
  timeoutMs: number,
): Promise<PreparedImages> {
  const images: NativeImage[] = [];
  const unavailable: string[] = [];
  let total = 0;
  for (const attachment of attachments) {
    const path = capturedPath(root, attachment);
    const header = await readPrefix(path, Math.min(attachment.size, HEADER_BYTES));
    if (
      header === undefined ||
      detectImage(header) === undefined ||
      attachment.size === 0 ||
      attachment.size > MAX_IMAGE_BYTES ||
      total + attachment.size > MAX_TOTAL_BYTES
    ) {
      unavailable.push(attachment.name);
      continue;
    }
    const bytes = await readPrefix(path, attachment.size);
    if (bytes?.length !== attachment.size)
      throw new ExplainedFailure(
        502,
        `The attached file ${attachment.name} could not be read; please send it again.`,
      );
    // The bytes uploaded are those core captured, and their own header gives
    // the type they are uploaded as; an image read before is no longer them.
    const detected =
      createHash("sha256").update(bytes).digest("hex") === attachment.sha256
        ? detectImage(bytes)
        : undefined;
    if (detected === undefined)
      throw new ExplainedFailure(
        502,
        `The attached file ${attachment.name} changed after Carillon received it; please send it again.`,
      );
    const image = {
      name: shortened(attachment.name.trim() || "image"),
      mimeType: detected.mediaType,
      sizeBytes: attachment.size,
    };
    let granted: unknown;
    try {
      granted = await connection.call(RPC.createUploadUrl, { type: "image", ...image });
    } catch (failure) {
      if (!(failure instanceof T3Failure) || failure.kind !== "rejected") throw failure;
      throw new ExplainedFailure(
        502,
        `T3 did not accept the attached image ${attachment.name}: ${failure.detail ?? failure.message}`,
      );
    }
    if (!Check(UploadUrlSchema, granted))
      throw new T3Failure(
        "malformed",
        "T3 answered an upload request in a form this runtime does not understand",
      );
    await send(
      endpoint,
      granted.relativeUrl,
      bytes,
      image.mimeType,
      signal,
      timeoutMs,
      attachment.name,
    );
    images.push({ type: "image", id: granted.attachmentId, ...image });
    total += attachment.size;
  }
  return { images, unavailable };
}

/** Posts the bytes to the signed upload URL, which must be on T3's own origin. */
async function send(
  endpoint: T3Endpoint,
  relativeUrl: string,
  bytes: Uint8Array,
  mimeType: string,
  signal: AbortSignal,
  timeoutMs: number,
  name: string,
): Promise<void> {
  const origin = new URL(endpoint.url);
  const target = new URL(relativeUrl, origin);
  if (
    !relativeUrl.startsWith("/") ||
    relativeUrl.startsWith("//") ||
    target.origin !== origin.origin
  )
    throw new T3Failure("malformed", "T3 offered an upload address outside its own origin");
  let response: Response;
  try {
    response = await fetch(target, {
      method: "POST",
      headers: { authorization: `Bearer ${endpoint.token}`, "content-type": mimeType },
      body: bytes,
      // A redirect would repeat the bytes wherever it points: it is refused below.
      redirect: "manual",
      signal: AbortSignal.any([signal, AbortSignal.timeout(timeoutMs)]),
    });
  } catch (failure) {
    signal.throwIfAborted();
    throw new T3Failure(
      failure instanceof Error && failure.name === "TimeoutError" ? "timeout" : "disconnected",
      "the image upload did not complete",
    );
  }
  await response.body?.cancel();
  if (response.status !== 204)
    throw new ExplainedFailure(
      502,
      `T3 did not accept the attached image ${name} (HTTP ${String(response.status)}).`,
    );
}

/** At most T3's name limit, without splitting a character. */
function shortened(name: string): string {
  let result = "";
  for (const character of name) {
    if (result.length + character.length > NAME_LIMIT) break;
    result += character;
  }
  return result;
}
