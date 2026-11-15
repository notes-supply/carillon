import type { Attachment } from "svelte/attachments";

export function focusedWhen(wanted: boolean): Attachment<HTMLElement> {
  return (element) => {
    if (wanted) element.focus();
  };
}
