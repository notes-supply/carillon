/**
 * Skills a person invokes in a message, as Pi's `/skill:name` does: the
 * message opening with `/skill:<name>`, or naming `$<name>` anywhere outside
 * code and quotes. Each skill the message invokes precedes it in Pi's skill
 * block, invocation-only ones too.
 */
import { stripFrontmatter, type Skill } from "@earendil-works/pi-coding-agent";
import type { RuntimeCommand } from "@carillon/protocol";
import { ExplainedFailure } from "@carillon/runtime/failure";
import { readFile } from "node:fs/promises";

const SKILL_COMMAND = "/skill:";
/** What `/command` lists a skill as, within the protocol's 100 UTF-16 units. */
const COMMAND_PREFIX = "skill:";
const COMMAND_NAME_LIMIT = 100;
/** A character that continues a word, so a name followed by one is not that name. */
const NAME_CHARACTER = /[\p{L}\p{N}_-]/u;
const WORD_CHARACTER = /[\p{L}\p{N}_$]/u;
const FENCE = /^ {0,3}(`{3,}|~{3,})/u;
const CLOSING_FENCE = /^ {0,3}(`{3,}|~{3,})[ \t]*$/u;
const QUOTED = /^ {0,3}>/u;
const CODE_SPAN = /(?<!`)(`+)(?!`)[\s\S]*?(?<!`)\1(?!`)/gu;

/**
 * The message with the skills it invokes before it. `/skill:<name> text`
 * leaves `text` as the message, refusing a name no skill has; `$<name>`
 * keeps the message whole. A message invoking none is returned as it is.
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
      `No skill is named "${name}". ${skills.length === 0 ? "The agent has no skills." : `The skills are ${skills.map((known) => known.name).join(", ")}.`}`,
    );
  return { skill, message: text.slice(SKILL_COMMAND.length + name.length).trim() };
}

/** The skills `$<name>` invokes, in the order first named, outside fenced code, code spans and quoted lines. */
function mentioned(text: string, skills: readonly Skill[]): Skill[] {
  const longestFirst = skills.toSorted((a, b) => b.name.length - a.name.length);
  const found: Skill[] = [];
  let fence: string | undefined;
  for (const line of text.split("\n")) {
    if (fence !== undefined) {
      const closing = CLOSING_FENCE.exec(line)?.[1];
      if (closing !== undefined && closing[0] === fence[0] && closing.length >= fence.length)
        fence = undefined;
      continue;
    }
    fence = FENCE.exec(line)?.[1];
    if (fence !== undefined || QUOTED.test(line)) continue;
    const prose = line.replace(CODE_SPAN, (span) => " ".repeat(span.length));
    for (let at = prose.indexOf("$"); at !== -1; at = prose.indexOf("$", at + 1)) {
      if (WORD_CHARACTER.test(prose[at - 1] ?? "")) continue;
      const skill = longestFirst.find(
        ({ name }) =>
          prose.startsWith(name, at + 1) && !NAME_CHARACTER.test(prose[at + 1 + name.length] ?? ""),
      );
      if (skill !== undefined) found.push(skill);
    }
  }
  return found;
}

/** The skill as Pi's `/skill:name` places it before a message. */
async function block(skill: Skill): Promise<string> {
  const body = stripFrontmatter(await readFile(skill.filePath, "utf8")).trim();
  return `<skill name="${skill.name}" location="${skill.filePath}">\nReferences are relative to ${skill.baseDir}.\n\n${body}\n</skill>`;
}
