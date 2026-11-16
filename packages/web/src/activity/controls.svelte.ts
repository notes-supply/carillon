import { SvelteMap } from "svelte/reactivity";
import { requestControl } from "../api/control.ts";
import type { Control, MessageSource } from "../api/stream.ts";

export type Target = { turnId: string; message: MessageSource; waiting: boolean };
type Offer = { action: Control; label: string; title: string; held: boolean };
type Offered = { offers: Offer[]; refusal: string };

type Press =
  | { kind: "asked"; action: Control; waiting: boolean; at: number }
  | { kind: "refused"; words: string };

const PRESSED_FOR_SECONDS = 15;

const WORDS: Record<
  Control,
  { label: string; pressed: string; waiting: string; answering: string }
> = {
  steer: {
    label: "Steer",
    pressed: "Steering",
    waiting: "Hand this message to the answer that is running.",
    answering: "Hand this message to the answer that is running.",
  },
  interrupt: {
    label: "Interrupt",
    pressed: "Interrupting",
    waiting: "End the running answer and run this message next.",
    answering: "End the running answer and run this message next.",
  },
  stop: {
    label: "Stop",
    pressed: "Stopping",
    waiting: "Do not run this message.",
    answering: "End this answer.",
  },
};

const presses = new SvelteMap<string, Press>();

function askedOf(
  press: Press | undefined,
  controls: readonly Control[],
  waiting: boolean,
  now: number,
): Control | undefined {
  if (press?.kind !== "asked" || press.waiting !== waiting) return undefined;
  const ignored = controls.includes(press.action) && now - press.at > PRESSED_FOR_SECONDS;
  return ignored ? undefined : press.action;
}

export function offered(target: Target, controls: readonly Control[], now: number): Offered {
  const press = presses.get(target.turnId);
  const asked = askedOf(press, controls, target.waiting, now);
  const listed = asked === undefined || controls.includes(asked) ? controls : [...controls, asked];
  return {
    offers: listed.map((action) => ({
      action,
      label: WORDS[action][asked === action ? "pressed" : "label"],
      title: WORDS[action][target.waiting ? "waiting" : "answering"],
      held: asked !== undefined,
    })),
    refusal: press?.kind === "refused" ? press.words : "",
  };
}

export async function press(
  target: Target,
  action: Control,
  token: string,
  now: number,
): Promise<void> {
  const asked: Press = { kind: "asked", action, waiting: target.waiting, at: now };
  presses.set(target.turnId, asked);
  const words = await requestControl(target.message, action, token);
  if (words !== "" && presses.get(target.turnId) === asked)
    presses.set(target.turnId, { kind: "refused", words });
}
