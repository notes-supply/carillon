import { ORIGIN } from "./origin.ts";
import { bearer, messageForm, messagePath, reading, refusalWords } from "./requests.ts";
import { retryPause } from "./retry.ts";
import type { ConversationKey } from "./stream.ts";

export type Outgoing = {
  key: ConversationKey;
  eventId: string;
  text: string;
  files: readonly File[];
};

export type Sending =
  | { kind: "uploading" }
  | { kind: "receiving" }
  | { kind: "retrying" }
  | { kind: "refused"; words: string };

type Answer = { status: number; body: string };

function put(
  path: string,
  body: FormData,
  token: string,
): { uploaded: Promise<void>; answered: Promise<Answer> } {
  const request = new XMLHttpRequest();
  request.open("PUT", ORIGIN + path);
  request.setRequestHeader("authorization", bearer(token));
  const uploaded = new Promise<void>((resolve) => {
    request.upload.addEventListener("loadend", () => resolve());
  });
  const answered = new Promise<Answer>((resolve) => {
    request.addEventListener("loadend", () =>
      resolve({ status: request.status, body: request.responseText }),
    );
  });
  request.send(body);
  return { uploaded, answered };
}

export async function* deliver(
  message: Outgoing,
  token: string,
): AsyncGenerator<Sending, void, void> {
  const path = messagePath(message.key, message.eventId);
  for (let failures = 1; ; failures += 1) {
    yield { kind: "uploading" };
    const request = put(path, messageForm(message.text, message.files), token);
    await Promise.race([request.uploaded, request.answered]);
    yield { kind: "receiving" };
    const answer = await request.answered;
    const read = reading(answer.status);
    if (read === "ok") return;
    if (read !== "away") {
      yield { kind: "refused", words: refusalWords(answer.status, answer.body) };
      return;
    }
    yield { kind: "retrying" };
    await retryPause(failures);
  }
}
