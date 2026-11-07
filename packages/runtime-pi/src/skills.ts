/**
 * Skills a person invokes in their own words, as Pi's `/skill:name` does:
 * their text opening with `/skill:<name>`, or naming `$<name>` anywhere
 * outside code. Each skill it invokes precedes it in Pi's skill block,
 * invocation-only ones too.
 */
import { stripFrontmatter, type Skill } from "@earendil-works/pi-coding-agent";
import { COMMAND_NAME_LIMIT, type RuntimeCommand } from "@carillon/protocol";
import { ExplainedFailure } from "@carillon/runtime/failure";
import { readFile } from "node:fs/promises";

const SKILL_COMMAND = "/skill:";
/** What `/command` lists a skill as. */
const COMMAND_PREFIX = "skill:";
/** A `$` that starts a word, unlike the one in `US$5` or `$$`. */
const SIGN = /(?<![\p{L}\p{N}_$])\$/gu;
/** A character that continues a name, so a name followed by one is not that name. */
const NAME_CHARACTER = /^[\p{L}\p{N}_-]/u;
/**
 * Code as Discord shows it, opened by the run of backticks in group 1: across
 * lines up to the next run of as many, backslashes inside kept as they are.
 * The other matches are text: a run never closed, and a backtick or backslash
 * escaped outside code.
 */
const CODE = /\\[\\`]|(`+)(?!`)[\s\S]*?(?<!`)\1(?!`)|`+/gu;

/**
 * The person's text with the skills it invokes before it. `/skill:<name>
 * text` leaves `text`, refusing a name no skill has; `$<name>` keeps the
 * text whole. Text invoking none is returned as it is.
 */
export async function expandSkills(text: string, skills: readonly Skill[]): Promise<string> {
  const command = text.startsWith(SKILL_COMMAND) ? commanded(text, skills) : undefined;
  const message = command?.message ?? text;
  const invoked = [
    ...new Set([...(command === undefined ? [] : [command.skill]), ...mentioned(message, skills)]),
  ];
  if (invoked.length === 0) return text;
  const blocks = await Promise.all(invoked.map(block));
  return [...blocks, ...(message.length === 0 ? [] : [message])].join("\n\n");
}

/** The agent's skills as the commands `/command` lists, invocation-only ones included. */
export function skillCommands(skills: readonly Skill[]): RuntimeCommand[] {
  return skills.flatMap(({ name, description }) => {
    const command = `${COMMAND_PREFIX}${name}`;
    if (command.length > COMMAND_NAME_LIMIT) return [];
    return [{ name: command, ...(description.length === 0 ? {} : { description }) }];
  });
}

function commanded(text: string, skills: readonly Skill[]): { skill: Skill; message: string } {
  const [name = ""] = text.slice(SKILL_COMMAND.length).split(/\s/u, 1);
  const skill = skills.find((candidate) => candidate.name === name);
  if (skill === undefined)
    throw new ExplainedFailure(
      422,
      `${name.length === 0 ? `Name a skill after ${SKILL_COMMAND}.` : `No skill is named "${name}".`} ${skills.length === 0 ? "The agent has no skills." : `The skills are ${skills.map((known) => known.name).join(", ")}.`}`,
    );
  return { skill, message: text.slice(SKILL_COMMAND.length + name.length).trim() };
}

/** The skills `$<name>` invokes outside code, in the order first named. */
function mentioned(text: string, skills: readonly Skill[]): Skill[] {
  const prose = text.replace(CODE, (match, run?: string) =>
    run === undefined ? match : " ".repeat(match.length),
  );
  const longestFirst = skills.toSorted((a, b) => b.name.length - a.name.length);
  return [...prose.matchAll(SIGN)].flatMap(({ index }) => {
    const skill = longestFirst.find(({ name }) => {
      const end = index + 1 + name.length;
      return prose.startsWith(name, index + 1) && !NAME_CHARACTER.test(prose.slice(end, end + 2));
    });
    return skill === undefined ? [] : [skill];
  });
}

/** The skill as Pi's `/skill:name` places it before a message. */
async function block(skill: Skill): Promise<string> {
  const file = await readFile(skill.filePath, "utf8").catch((failure: unknown) => {
    throw new ExplainedFailure(
      422,
      `The skill ${skill.name} could not be read: ${failure instanceof Error ? failure.message : String(failure)}`,
    );
  });
  const body = stripFrontmatter(file).trim();
  return `<skill name="${skill.name}" location="${skill.filePath}">\nReferences are relative to ${skill.baseDir}.\n\n${body}\n</skill>`;
}
