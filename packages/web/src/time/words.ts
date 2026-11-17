export function span(millis: number): string {
  if (millis < 1000) return millis <= 0 ? "0 s" : `${String(Math.round(millis))} ms`;
  const seconds = Math.floor(millis / 1000);
  if (seconds < 60) return `${String(seconds)} s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${String(minutes)} min ${String(seconds % 60)} s`;
  return `${String(Math.floor(minutes / 60))} h ${String(minutes % 60)} min`;
}

export function when(moment: number, now: number, locale?: string): string {
  const date = new Date(moment * 1000);
  const today = new Date(now * 1000);
  if (date.toDateString() === today.toDateString())
    return date.toLocaleTimeString(locale, { timeStyle: "short" });
  return date.toLocaleDateString(locale, {
    day: "numeric",
    month: "short",
    ...(date.getFullYear() === today.getFullYear() ? {} : { year: "numeric" }),
  });
}
