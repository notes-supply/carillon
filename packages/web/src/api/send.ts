import type { Credential } from "./credential.ts";
import { bearer, messageForm, messagePath, reading, refusalWords } from "./requests.ts";
import { retryPause } from "./retry.ts";
import type { ConversationKey } from "./stream.ts";

export type Outgoing = {
  key: ConversationKey;
  eventId: string;
  text: string;
  files: readonly File[];
};

type Leaving = { kind: "uploading"; sent: number; total: number } | { kind: "receiving" };

export type Sending =
  | Leaving
  | { kind: "retrying" }
  | { kind: "received" }
  | { kind: "refused"; words: string }
  | { kind: "cancelled" };

type Answer = { status: number; body: string };

function put(
  path: string,
  body: FormData,
  token: string,
  signal: AbortSignal,
): { leaving: ReadableStream<Leaving>; answered: Promise<Answer> } {
  const request = new XMLHttpRequest();
  const cancel = (): void => request.abort();
  request.open("PUT", path);
  request.setRequestHeader("authorization", bearer(token));
  const leaving = new ReadableStream<Leaving>({
    start(steps) {
      request.upload.addEventListener("progress", ({ loaded, total }) =>
        steps.enqueue({ kind: "uploading", sent: loaded, total }),
      );
      request.upload.addEventListener("load", () => {
        if (request.readyState !== XMLHttpRequest.DONE) steps.enqueue({ kind: "receiving" });
      });
      request.addEventListener("loadend", () => steps.close());
    },
  });
  const answered = new Promise<Answer>((resolve) => {
    request.addEventListener("loadend", () => {
      signal.removeEventListener("abort", cancel);
      resolve({ status: request.status, body: request.responseText });
    });
  });
  signal.addEventListener("abort", cancel);
  request.send(body);
  return { leaving, answered };
}

export async function* deliver(
  message: Outgoing,
  credential: Credential,
  signal: AbortSignal,
): AsyncGenerator<Sending, void, void> {
  const path = messagePath(message.key, message.eventId);
  for (let failures = 1; ; failures += 1) {
    yield { kind: "uploading", sent: 0, total: 0 };
    const request = put(path, messageForm(message.text, message.files), credential.token, signal);
    const leaving = request.leaving.getReader();
    for (let step = await leaving.read(); !step.done; step = await leaving.read()) yield step.value;
    const answer = await request.answered;
    const read = reading(answer.status);
    if (read === "ok") {
      yield { kind: "received" };
      return;
    }
    if (read === "signed_out") credential.refused();
    if (signal.aborted) break;
    if (read !== "away") {
      yield { kind: "refused", words: refusalWords(answer.status, answer.body) };
      return;
    }
    yield { kind: "retrying" };
    await retryPause(failures, signal);
    if (signal.aborted) break;
  }
  yield { kind: "cancelled" };
}
