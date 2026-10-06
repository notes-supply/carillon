import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { isIP } from "node:net";
import { Type, type Static } from "typebox";
import { Check } from "typebox/value";

const id = Type.String({ pattern: "^[1-9][0-9]{0,19}$" });
const Origin = Type.Object({
  guild_id: id,
  channel_id: id,
  source_channel_id: id,
  message_id: id,
  user_id: id,
  parent_channel_id: Type.Optional(Type.Union([id, Type.Null()])),
});
const Context = Type.Object({
  interface_id: Type.String(),
  connection: Type.Object({
    url: Type.String(),
    token_file: Type.String(),
    token_hash: Type.String(),
  }),
  native: Type.Object({ origin: Type.Optional(Origin) }),
});
export type Origin = Static<typeof Origin>;

export class DiscordFailure extends Error {
  readonly outcome: "rejected" | "unavailable" | "uncertain";
  constructor(outcome: "rejected" | "unavailable" | "uncertain", message: string) {
    super(message);
    this.outcome = outcome;
  }
}
const Failure = Type.Object({
  outcome: Type.Union([
    Type.Literal("rejected"),
    Type.Literal("unavailable"),
    Type.Literal("uncertain"),
  ]),
  message: Type.String({ maxLength: 2000 }),
});

/**
 * The provider's operations endpoint, when `endpoint` is an HTTP loopback
 * socket address, as the installation registry records providers. The
 * operator credential is never read for, or sent to, anything else.
 */
function operationsEndpoint(endpoint: string): URL | undefined {
  if (!/^http:\/\/(?:\[[^\]]+\]|[^:/@?#]+):\d+$/u.test(endpoint)) return undefined;
  let url: URL;
  try {
    url = new URL(endpoint);
  } catch {
    return undefined;
  }
  const host = url.hostname.replace(/^\[|\]$/gu, "");
  const loopback = (isIP(host) === 4 && host.startsWith("127.")) || host === "::1";
  return loopback && Number(url.port || "80") !== 0 ? new URL("/v1/operations", url) : undefined;
}

/** A connection is explicit session data, not process globals or model arguments. */
export class DiscordClient {
  private readonly context: unknown;
  constructor(context: unknown) {
    this.context = context;
  }

  get origin(): Origin | undefined {
    return Check(Context, this.context) ? this.context.native.origin : undefined;
  }

  async call(operation: Record<string, unknown>, signal?: AbortSignal): Promise<unknown> {
    const context = this.context;
    if (!Check(Context, context))
      throw new DiscordFailure(
        "unavailable",
        "This session has no configured interface connection. Use an installed Discord conversation or supply explicit extension context.",
      );
    const url = operationsEndpoint(context.connection.url);
    if (url === undefined)
      throw new DiscordFailure("unavailable", "Invalid configured interface endpoint.");
    let token: string;
    try {
      token = (await readFile(context.connection.token_file, "utf8")).trim();
    } catch {
      throw new DiscordFailure(
        "unavailable",
        "Cannot read the local operator credential. Check the installation.",
      );
    }
    if (
      token.length === 0 ||
      createHash("sha256").update(token).digest("hex") !== context.connection.token_hash
    )
      throw new DiscordFailure(
        "unavailable",
        "The local operator credential does not match the installation.",
      );
    signal?.throwIfAborted();
    const read =
      typeof operation["action"] === "string" && /\.(info|list|get)$/.test(operation["action"]);
    const uncertain = () =>
      new DiscordFailure(
        read ? "unavailable" : "uncertain",
        read
          ? "The Discord provider did not complete this read."
          : "The Discord provider did not confirm the operation. It may have taken effect; inspect the target before repeating it.",
      );
    try {
      const response = await fetch(url, {
        method: "POST",
        redirect: "error",
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
        body: JSON.stringify(operation),
        signal: AbortSignal.any([AbortSignal.timeout(35_000), ...(signal ? [signal] : [])]),
      });
      // The provider is trusted, but a bounded body protects the model context and memory.
      const reader = response.body?.getReader();
      if (!reader) throw uncertain();
      const chunks: Uint8Array[] = [];
      let size = 0;
      try {
        for (;;) {
          const part = await reader.read();
          if (part.done) break;
          const chunk: unknown = part.value;
          if (!(chunk instanceof Uint8Array)) throw uncertain();
          size += chunk.byteLength;
          if (size > 2_097_152) throw uncertain();
          chunks.push(chunk);
        }
      } finally {
        await reader.cancel();
      }
      const body: unknown = JSON.parse(Buffer.concat(chunks).toString("utf8"));
      if (!response.ok) {
        if (Check(Failure, body)) throw new DiscordFailure(body.outcome, body.message);
        if ([400, 401, 403, 404, 413].includes(response.status))
          throw new DiscordFailure(
            "rejected",
            `The local provider rejected this request (HTTP ${String(response.status)}). Check the installation and tool arguments.`,
          );
        throw uncertain();
      }
      return body;
    } catch (failure) {
      if (failure instanceof DiscordFailure) throw failure;
      throw uncertain();
    }
  }
}
