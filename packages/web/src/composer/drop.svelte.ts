import type { Attachment } from "svelte/attachments";

export function takesDrops(take: (files: readonly File[]) => void): Attachment<HTMLElement> {
  return (element) => {
    const page = element.ownerDocument;
    const over = (event: DragEvent): void => event.preventDefault();
    const dropped = (event: DragEvent): void => {
      event.preventDefault();
      take([...(event.dataTransfer?.files ?? [])]);
    };
    page.addEventListener("dragover", over);
    page.addEventListener("drop", dropped);
    return () => {
      page.removeEventListener("dragover", over);
      page.removeEventListener("drop", dropped);
    };
  };
}
