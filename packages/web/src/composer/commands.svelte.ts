import { SvelteMap } from "svelte/reactivity";
import { requestCommands } from "../api/commands.ts";
import type { Command } from "../api/requests.ts";
import type { ConversationKey } from "../api/stream.ts";
import { addressOf } from "../session/address.ts";

const NO_COMMANDS: readonly Command[] = [];

const catalogs = new SvelteMap<string, readonly Command[]>();
const asked = new Set<string>();

export async function loadCommands(key: ConversationKey, token: string | null): Promise<void> {
  const address = addressOf(key);
  if (token === null || asked.has(address)) return;
  asked.add(address);
  const commands = await requestCommands(key, token);
  if (commands === undefined) asked.delete(address);
  else catalogs.set(address, commands);
}

export function offered(address: string, text: string): readonly Command[] {
  if (!text.startsWith("/")) return NO_COMMANDS;
  const begun = text.slice(1);
  return (catalogs.get(address) ?? NO_COMMANDS).filter((command) => command.name.startsWith(begun));
}
