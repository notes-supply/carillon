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
  token: string,
): Promise<readonly Command[] | undefined> {
  const answer = await fetch(ORIGIN + commandsPath(key), {
    headers: { authorization: bearer(token) },
  })
    .then(async (response) => ({ status: response.status, body: await response.text() }))
    .catch(() => ({ status: 0, body: "" }));
  if (reading(answer.status) !== "ok") return undefined;
  return readAnswer(CommandCatalogSchema, answer.body)?.commands;
}
