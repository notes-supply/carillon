const PREFIX = "draft:";

function attempted<Value>(use: () => Value): Value | undefined {
  try {
    return use();
  } catch {
    return undefined;
  }
}

export function addressOfDraft(stored: string | null): string | undefined {
  return stored?.startsWith(PREFIX) === true ? stored.slice(PREFIX.length) : undefined;
}

export function storedDraft(address: string): string {
  return attempted(() => localStorage.getItem(PREFIX + address)) ?? "";
}

export function storeDraft(address: string, text: string): void {
  const stored = PREFIX + address;
  const kept = attempted(() => {
    if (text !== "") localStorage.setItem(stored, text);
    return text !== "";
  });
  if (kept !== true) attempted(() => localStorage.removeItem(stored));
}
