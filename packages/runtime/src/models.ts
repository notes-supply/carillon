/**
 * Selections checked against a runtime's catalogue. Each runtime owns its
 * catalogue and where selections live; what a selection means against the
 * catalogue is the same for all of them.
 */
import type { AvailableModel, ModelOption, Selection } from "@carillon/protocol";
import { ExplainedFailure } from "./failure.ts";

export function offeredModel(
  available: readonly AvailableModel[],
  selection: Selection,
): AvailableModel | undefined {
  return available.find(
    (candidate) => candidate.provider === selection.provider && candidate.model === selection.model,
  );
}

/**
 * The selection as it runs: every option the model supports, with the chosen
 * value where it is still supported and the model's default elsewhere. A
 * model the catalogue no longer offers is returned as chosen; a turn on it
 * fails rather than substituting another model.
 */
export function resolved(available: readonly AvailableModel[], selection: Selection): Selection {
  const model = offeredModel(available, selection);
  if (model === undefined) return selection;
  const options: Record<string, string> = {};
  for (const option of model.options) {
    const choice = selection.options?.[option.id];
    options[option.id] = choice !== undefined && offers(option, choice) ? choice : option.default;
  }
  return { provider: model.provider, model: model.model, options };
}

/** A selection the catalogue supports, with unset options omitted; refused otherwise. */
export function validated(available: readonly AvailableModel[], selection: Selection): Selection {
  const model = offeredModel(available, selection);
  if (model === undefined) throw new ExplainedFailure(422, "That model is not available.");
  const options: Record<string, string> = {};
  for (const [id, choice] of Object.entries(selection.options ?? {})) {
    const option = model.options.find((candidate) => candidate.id === id);
    if (option === undefined)
      throw new ExplainedFailure(422, `${model.name} has no ${id} setting.`);
    if (!offers(option, choice))
      throw new ExplainedFailure(
        422,
        `${choice} is not a ${option.name} setting ${model.name} supports.`,
      );
    options[id] = choice;
  }
  return { provider: model.provider, model: model.model, options };
}

function offers(option: ModelOption, choice: string): boolean {
  return option.choices.some((candidate) => candidate.id === choice);
}
