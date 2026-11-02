/** Reading the replies and work cards the Discord module draws. */
import assert from "node:assert/strict";

export function embedText(embed) {
  assert.ok(embed, "the job has no work card");
  return [
    embed.description,
    ...(embed.fields ?? []).map(({ name, value }) => `${name}:\n${value}`),
  ].join("\n");
}

export function seen(reply) {
  return [
    reply.content,
    ...reply.embeds.flatMap((embed) => [
      embed.description,
      ...(embed.fields ?? []).flatMap((field) => [field.name, field.value]),
      embed.footer?.text,
    ]),
  ]
    .filter(Boolean)
    .join("\n");
}

/** The first control of each action row. */
export function customIds(reply) {
  return reply.components.map((row) => row.components[0].custom_id);
}

function control(reply, customId) {
  const found = reply.components
    .flatMap((row) => row.components)
    .find((component) => component.custom_id === customId);
  assert.ok(found, `${customId} is present`);
  return found;
}

export function options(reply, customId) {
  return control(reply, customId).options;
}

/** The select value standing for the option labelled or described `text`. */
export function value(reply, customId, text) {
  const option = options(reply, customId).find(
    (candidate) => candidate.label === text || candidate.description === text,
  );
  assert.ok(option, `${customId} offers ${text}`);
  return option.value;
}
