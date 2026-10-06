/**
 * The commands a person may run in a T3 conversation: the slash commands
 * and user-invocable skills T3 lists for the conversation's provider
 * instance in the agent's workspace, as its own composer offers them.
 * Invoking one is sending its text; T3 runs it.
 */
import type { CommandCatalog, RuntimeCommand } from "@carillon/protocol";
import { Check } from "typebox/value";
import {
  ProviderCommandsSchema,
  ServerConfigSchema,
  SkillSchema,
  SlashCommandSchema,
} from "./contracts.ts";
import { T3Failure } from "./t3.ts";

/** The longest name an interface can carry whole (`RuntimeCommandSchema`). */
const NAME_LIMIT = 100;

/**
 * The instance's commands, then its skills, for the workspace when T3 has
 * scanned it, else machine-wide. A name listed twice is offered once.
 */
export function commandCatalog(
  providers: unknown,
  instanceId: string,
  workspace: string,
): CommandCatalog {
  if (!Check(ServerConfigSchema, providers))
    throw new T3Failure(
      "malformed",
      "T3 described its providers in a form this runtime does not understand",
    );
  const provider = providers.providers.find(
    (candidate) => Check(ProviderCommandsSchema, candidate) && candidate.instanceId === instanceId,
  );
  if (provider === undefined || !Check(ProviderCommandsSchema, provider)) return { commands: [] };
  const scanned = provider.workspaceSnapshots?.find((snapshot) => snapshot.cwd === workspace);
  const listed = new Map<string, RuntimeCommand>();
  const offer = (command: RuntimeCommand) => {
    if (command.name.length <= NAME_LIMIT && !listed.has(command.name))
      listed.set(command.name, command);
  };
  for (const command of scanned?.slashCommands ?? provider.slashCommands ?? [])
    if (Check(SlashCommandSchema, command)) {
      const description = present(command.description);
      const hint = present(command.input?.hint);
      offer({
        name: command.name,
        ...(description === undefined ? {} : { description }),
        ...(hint === undefined ? {} : { hint }),
      });
    }
  for (const skill of scanned?.skills ?? provider.skills ?? [])
    if (Check(SkillSchema, skill) && skill.enabled && skill.userInvocable !== false) {
      const description = present(skill.description);
      offer({ name: skill.name, ...(description === undefined ? {} : { description }) });
    }
  return { commands: [...listed.values()] };
}

function present(value: string | undefined): string | undefined {
  const trimmed = value?.trim();
  return trimmed === undefined || trimmed.length === 0 ? undefined : trimmed;
}
