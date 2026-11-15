import type { Attachment } from "svelte/attachments";

const NEAR = 48;

function end(transcript: HTMLElement): number {
  const last = transcript.scrollHeight - transcript.clientHeight;
  const newest = transcript.querySelector(".turns > :last-child")?.getBoundingClientRect();
  if (newest === undefined || newest.height <= transcript.clientHeight) return last;
  const top = transcript.scrollTop + newest.top - transcript.getBoundingClientRect().top;
  return Math.min(top, last);
}

export const followEnd: Attachment<HTMLElement> = (transcript) => {
  let following = true;
  const scrolled = (): void => {
    following = end(transcript) - transcript.scrollTop < NEAR;
  };
  const resized = new ResizeObserver(() => {
    if (following) transcript.scrollTop = end(transcript);
  });
  transcript.addEventListener("scroll", scrolled);
  resized.observe(transcript);
  for (const part of transcript.children) resized.observe(part);
  return () => {
    transcript.removeEventListener("scroll", scrolled);
    resized.disconnect();
  };
};
