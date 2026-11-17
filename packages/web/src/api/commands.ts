import type { Credential } from "./credential.ts";
import { ORIGIN } from "./origin.ts";
import {
  CommandCatalogSchema,
  bearer,
  commandsPath,
  readAnswer,
  reading,
  type Command,
} from "./requests.ts";
import type { ConversationKey } from "./stream.ts";

export async function requestCommands(
  key: ConversationKey,
  credential: Credential,
): Promise<readonly Command[] | undefined> {
  const answer = await fetch(ORIGIN + commandsPath(key), {
    headers: { authorization: bearer(credential.token) },
  })
    .then(async (response) => ({ status: response.status, body: await response.text() }))
    .catch(() => ({ status: 0, body: "" }));
  const read = reading(answer.status);
  if (read === "signed_out") credential.refused();
  if (read !== "ok") return undefined;
  return readAnswer(CommandCatalogSchema, answer.body)?.commands;
}
