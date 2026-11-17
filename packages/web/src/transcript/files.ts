const UNITS = ["B", "kB", "MB", "GB"] as const;

export function size(bytes: number): string {
  let value = bytes;
  let unit = 0;
  while (value >= 1000 && unit < UNITS.length - 1) {
    value /= 1000;
    unit += 1;
  }
  const shown = value < 10 && unit > 0 ? Math.round(value * 10) / 10 : Math.round(value);
  return `${String(shown)} ${UNITS[unit] ?? "B"}`;
}
