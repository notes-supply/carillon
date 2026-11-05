/**
 * The variables tests keep from whoever runs them. Any other, such as a model
 * provider's credentials or endpoint, or a Pi or Carillon home, would let the
 * caller's setup decide what the programs a test starts do.
 */
const KEPT = new Set([
  "PATH",
  "HOME",
  "USER",
  "LOGNAME",
  "SHELL",
  "TMPDIR",
  "LANG",
  "LANGUAGE",
  "TZ",
  "TERM",
  "CI",
  "NO_COLOR",
  "FORCE_COLOR",
  "NODE_OPTIONS",
  "RUST_BACKTRACE",
]);

/** Leaves this process, and every program it starts after, only the variables tests keep. */
export function isolateEnvironment(): void {
  for (const name of Object.keys(process.env))
    if (!KEPT.has(name) && !name.startsWith("LC_") && !name.startsWith("VITEST"))
      delete process.env[name];
}
