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

export function onDesktop(): boolean {
  return matchMedia("(min-width: 46rem)").matches;
}

let message: HTMLElement | undefined;

export const messageField: Attachment<HTMLElement> = (element) => {
  message = element;
  return () => {
    if (message === element) message = undefined;
  };
};

export function focusMessage(): void {
  if (onDesktop()) message?.focus();
}
