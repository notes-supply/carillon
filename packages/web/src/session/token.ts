const STORED = "token";

export function storedToken(): string | null {
  try {
    return localStorage.getItem(STORED);
  } catch {
    return null;
  }
}

export function storeToken(token: string): boolean {
  try {
    localStorage.setItem(STORED, token);
    return true;
  } catch {
    return false;
  }
}

export function dropToken(): boolean {
  try {
    localStorage.removeItem(STORED);
    return true;
  } catch {
    return false;
  }
}

export function namesToken(key: string | null): boolean {
  return key === STORED;
}
