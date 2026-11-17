import type { Attachment } from "svelte/attachments";

const NEAR = 48;

type Held = { turn: Element; top: number };

function end(transcript: HTMLElement): number {
  const last = transcript.scrollHeight - transcript.clientHeight;
  const newest = transcript.querySelector(".turns > :last-child")?.getBoundingClientRect();
  if (newest === undefined || newest.height <= transcript.clientHeight) return last;
  const top = transcript.scrollTop + newest.top - transcript.getBoundingClientRect().top;
  return Math.min(top, last);
}

function oldest(transcript: HTMLElement): Held | undefined {
  const turn = transcript.querySelector(".turns > :first-child");
  return turn === null ? undefined : { turn, top: turn.getBoundingClientRect().top };
}

function pushed(held: Held | undefined): number {
  return held?.turn.isConnected === true ? held.turn.getBoundingClientRect().top - held.top : 0;
}

export const followEnd: Attachment<HTMLElement> = (transcript) => {
  let following = true;
  let held = oldest(transcript);
  const scrolled = (): void => {
    const top = transcript.scrollTop;
    const bottom = transcript.scrollHeight - transcript.clientHeight;
    following = Math.abs(end(transcript) - top) < NEAR || bottom - top < NEAR;
    held = oldest(transcript);
  };
  const resized = new ResizeObserver(() => {
    transcript.scrollTop = following ? end(transcript) : transcript.scrollTop + pushed(held);
    held = oldest(transcript);
  });
  transcript.addEventListener("scroll", scrolled);
  resized.observe(transcript);
  for (const part of transcript.children) resized.observe(part);
  return () => {
    transcript.removeEventListener("scroll", scrolled);
    resized.disconnect();
  };
};
