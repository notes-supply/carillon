import type { Attachment } from "svelte/attachments";

export function focusedWhen(wanted: boolean): Attachment<HTMLElement> {
  return (element) => {
    if (wanted) element.focus();
  };
}

export function inViewWhen(wanted: boolean): Attachment<HTMLElement> {
  return (element) => {
    if (wanted) element.scrollIntoView({ block: "nearest" });
  };
}

const DESKTOP = "(min-width: 46rem)";

let message: HTMLElement | undefined;

export const messageField: Attachment<HTMLElement> = (element) => {
  message = element;
  return () => {
    if (message === element) message = undefined;
  };
};

export function focusMessage(): void {
  if (matchMedia(DESKTOP).matches) message?.focus();
}
