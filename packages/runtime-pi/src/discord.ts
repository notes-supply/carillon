import { join } from "node:path";
import type { Installation, RegisteredProvider } from "@carillon/runtime/installation";

type Registry = Pick<Installation, "root" | "interfaces" | "operatorTokenHash">;

function discordProviders(config: Registry): RegisteredProvider[] {
  return config.interfaces.filter((provider) => provider.program === "discord");
}

/** The built-in extensions an agent has unless its settings say otherwise. */
export function defaultExtensions(config: Registry): string[] {
  return ["coding", "tool-search", ...(discordProviders(config).length > 0 ? ["discord"] : [])];
}

/**
 * What the Discord tools learn of `conversation`: host data, never the
 * model's. A Discord conversation reaches its own provider and knows its
 * message's origin. Any other reaches the only Discord provider, without an
 * origin, so the agent names the channel it means.
 */
export function discordContext(
  config: Registry,
  conversation: { interface_id: string; agent_id: string; native?: unknown } | undefined,
): unknown {
  const providers = discordProviders(config);
  const own = providers.find((provider) => provider.id === conversation?.interface_id);
  const provider = own ?? (providers.length === 1 ? providers[0] : undefined);
  return {
    interface_id: provider?.id,
    agent_id: conversation?.agent_id,
    native: own === undefined ? {} : conversation?.native,
    connection:
      provider === undefined
        ? undefined
        : {
            url: provider.url,
            token_file: join(config.root, "credentials", "operator.token"),
            token_hash: config.operatorTokenHash,
          },
  };
}
