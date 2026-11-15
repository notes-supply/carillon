function stored(address: string): string {
  return `draft:${address}`;
}

export function storedDraft(address: string): string {
  return localStorage.getItem(stored(address)) ?? "";
}

export function storeDraft(address: string, text: string): void {
  if (text === "") localStorage.removeItem(stored(address));
  else localStorage.setItem(stored(address), text);
}
