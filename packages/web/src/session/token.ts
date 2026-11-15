const STORED = "token";

export function storedToken(): string | null {
  return localStorage.getItem(STORED);
}

export function storeToken(token: string): void {
  localStorage.setItem(STORED, token);
}

export function dropToken(): void {
  localStorage.removeItem(STORED);
}
