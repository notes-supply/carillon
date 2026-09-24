/** What the runtime tells callers when work fails, and what it keeps in its own log. */

/**
 * A failure explained for the person using the interface. The HTTP adapter
 * sends only these messages to callers, as the response's `error`, so each
 * must already be safe to display. Any other failure is logged and reported
 * without an explanation. `cause` carries diagnostics for the log.
 */
export class ExplainedFailure extends Error {
  readonly status: number;

  constructor(status: number, message: string, options?: ErrorOptions) {
    super(message, options);
    this.status = status;
  }
}

/** The most an explanation shows. The full diagnostic stays in the runtime log. */
export const EXPLANATION_LIMIT = 300;

/**
 * Reduces failure text composed by someone else (a model provider, an SDK) to
 * an explanation that is safe to show in a conversation, or undefined when
 * nothing in it is.
 *
 * Such text is meant for a person but is often assembled from HTTP responses:
 * it can be a raw JSON body, an HTML page from a proxy, or carry appended
 * metadata lines and echoed request details. Only the first line counts; a
 * JSON body contributes just its `message`; markup is refused; credential-like
 * runs are redacted; and the result is bounded.
 */
export function displayableExplanation(text: string): string | undefined {
  const line = withoutJsonBody(text.split(/\r\n|\r|\n/u, 1)[0] ?? "");
  if (line === undefined || /<[A-Za-z!?/]/u.test(line)) return undefined;
  const explanation = redactCredentials(line.replace(/\p{Cc}/gu, " "))
    .split(/\s+/u)
    .filter((word) => word.length > 0)
    .join(" ");
  return explanation.length === 0 ? undefined : bounded(explanation);
}

/**
 * Replaces long unbroken runs of token characters: API keys, bearer tokens,
 * JWT segments and signed URL parameters, which prose, model names and file
 * paths do not contain. Used for both explanations and logged diagnostics.
 */
export function redactCredentials(text: string): string {
  return text.replace(/[A-Za-z0-9_+=-]{32,}/gu, "[redacted]");
}

/** A diagnostic for the runtime log: the stack or message, never the error object and its request data. */
export function diagnostic(failure: unknown): string {
  const detail = failure instanceof Error ? (failure.stack ?? failure.message) : String(failure);
  return redactCredentials(detail);
}

function withoutJsonBody(line: string): string | undefined {
  const start = line.indexOf("{");
  if (start === -1) return line;
  let body: unknown;
  try {
    body = JSON.parse(line.slice(start));
  } catch {
    return undefined;
  }
  const message = bodyMessage(body);
  return message === undefined ? undefined : `${line.slice(0, start)}${message}`;
}

/** The shallowest string `message` in a JSON error body, the common convention for its human part. */
function bodyMessage(body: unknown): string | undefined {
  const pending: unknown[] = [body];
  for (const value of pending) {
    if (typeof value !== "object" || value === null) continue;
    if ("message" in value && typeof value.message === "string") return value.message;
    const nested: unknown[] = Object.values(value);
    pending.push(...nested);
  }
  return undefined;
}

/** Cut between graphemes, at a word where there is one, with length in UTF-16 units: never more characters than Discord counts. */
function bounded(text: string): string {
  if (text.length <= EXPLANATION_LIMIT) return text;
  let kept = "";
  for (const { segment } of new Intl.Segmenter().segment(text)) {
    if (kept.length + segment.length >= EXPLANATION_LIMIT) break;
    kept += segment;
  }
  const space = kept.lastIndexOf(" ");
  return `${space > 0 ? kept.slice(0, space) : kept}…`;
}
