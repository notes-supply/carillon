import { turnIntent } from "@carillon/protocol";
import type { TurnRequest } from "@carillon/runtime/operations";

/**
 * The message T3 is sent for the turn: a command invocation verbatim, so
 * T3 runs it as its own; else the person's text as written, followed by
 * Carillon's notes of the files T3 cannot take, of this message and of
 * earlier ones.
 */
export function compose(request: TurnRequest, unavailable: readonly string[]): string {
  if (turnIntent(request.native) === "command") return request.input;
  const earlier = (request.unanswered ?? []).flatMap((turn) =>
    turn.attachments.map((attachment) => attachment.name),
  );
  const notes = [
    ...(unavailable.length === 0
      ? []
      : [
          `[Carillon: ${String(unavailable.length)} file(s) attached to this message are not available to this runtime: ${unavailable.join(", ")}]`,
        ]),
    ...(earlier.length === 0
      ? []
      : [
          `[Carillon: ${String(earlier.length)} file(s) attached to earlier messages are not available to this runtime: ${earlier.join(", ")}]`,
        ]),
  ];
  return [request.input, ...notes].filter((part) => part.length > 0).join("\n\n");
}

/** The person's words a plain T3 Code thread's first message is titled from, as T3's composer seeds it. */
export function titleSeed(input: string): string | undefined {
  const words = input
    .split(/\s+/u)
    .filter((word) => word.length > 0)
    .join(" ");
  return words.length === 0 ? undefined : words.slice(0, 80);
}
