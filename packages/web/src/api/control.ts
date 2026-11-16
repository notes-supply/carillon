import { ORIGIN } from "./origin.ts";
import { bearer, controlBody, controlPath, reading, refusalWords } from "./requests.ts";
import type { Control, MessageSource } from "./stream.ts";

const UNANSWERED = "Carillon did not answer. If the turn goes on, press it again.";

export async function requestControl(
  message: MessageSource,
  action: Control,
  token: string,
): Promise<string> {
  const answer = await fetch(ORIGIN + controlPath(message), {
    method: "POST",
    headers: { authorization: bearer(token), "content-type": "application/json" },
    body: controlBody(action),
  })
    .then(async (response) => ({ status: response.status, body: await response.text() }))
    .catch(() => ({ status: 0, body: "" }));
  const read = reading(answer.status);
  if (read === "ok") return "";
  return read === "away" ? UNANSWERED : refusalWords(answer.status, answer.body);
}
