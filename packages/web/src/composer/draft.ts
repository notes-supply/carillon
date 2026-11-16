const PREFIX = "draft:";

export function addressOfDraft(stored: string | null): string | undefined {
  return stored?.startsWith(PREFIX) === true ? stored.slice(PREFIX.length) : undefined;
}

export function storedDraft(address: string): string {
  return localStorage.getItem(PREFIX + address) ?? "";
}

export function storeDraft(address: string, text: string): void {
  if (text === "") localStorage.removeItem(PREFIX + address);
  else localStorage.setItem(PREFIX + address, text);
}
