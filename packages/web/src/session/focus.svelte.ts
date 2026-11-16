import type { Attachment } from "svelte/attachments";

export function focusedWhen(wanted: boolean): Attachment<HTMLElement> {
  return (element) => {
    if (wanted) element.focus();
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
