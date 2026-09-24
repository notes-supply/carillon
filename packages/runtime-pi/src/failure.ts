/**
 * What the runtime tells callers when work fails, and what it keeps in its own log.
 *
 * Failure text often comes from someone else (a model provider, an SDK, a
 * request) and can quote credentials. These rules are conservative and
 * best-effort: they withhold or redact the structures that carry credentials,
 * whatever their length, but cannot prove arbitrary prose secret-free.
 */

/**
 * A failure whose message is meant for the person using the interface, such as
 * a model provider's explanation or a refused model change. The message may
 * quote external or user-supplied text: the HTTP adapter sends only what
 * {@link displayableExplanation} keeps of it. Any other failure is internal and
 * reported without an explanation.
 */
export class ExplainedFailure extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

/** The most an explanation shows. The full diagnostic stays in the runtime log. */
export const EXPLANATION_LIMIT = 300;

// A word naming a credential. Letters may not touch it, so `access_token` and
// `x-api-key` count, while `tokenizer` does not.
const CREDENTIAL = String.raw`(?<![A-Za-z])(?:api[-_ ]?keys?|access[-_ ]?keys?|private[-_ ]?keys?|keys?|secrets?|passwords?|passphrases?|passwd|tokens?|authorization|cookies?|credentials?|signatures?)(?![A-Za-z])`;
// That word labelling a value: `password: x`, `"token":"x"`, `API key provided: x`.
const LABELLED = new RegExp(
  String.raw`(${CREDENTIAL}["']?(?:[ \t]+[^\s:=]+)?[ \t]*[:=][ \t]*)("[^"]*"|'[^']*'|[^\s,;]+)`,
  "giu",
);
const ASSIGNMENT = /([^\s=&?]+=)("[^"]*"|'[^']*'|[^\s&,;]+)/gu;
const AUTH_SCHEME = /((?<![A-Za-z])(?:bearer|basic)[ \t]+)(\S+)/giu;
const URL_PARTS = /([a-z][a-z0-9+.-]*:\/\/)(?:[^\s/?#@]*@)?([^\s?#]*)([?#]\S*)?/giu;
const USERINFO = /[^\s:/@]+:[^\s/@]*@[^\s@]/u;
// Unlabelled long runs of token characters: keys, JWT segments, signatures.
const OPAQUE = /[A-Za-z0-9_+=-]{32,}/gu;

/**
 * What of failure text is safe to show in a conversation, or undefined when
 * nothing is. Only the first line counts, and a JSON error body contributes
 * only its conventional `message`, `error` or `error.message`. The whole
 * explanation is withheld if it contains markup, a link target, an assignment,
 * a labelled credential, URL user information or an authorization scheme.
 * URLs become `[link]`, long opaque runs `[redacted]`, and the result is bounded.
 */
export function displayableExplanation(text: string): string | undefined {
  const line = withoutJsonBody(text.split(/\r\n|\r|\n/u, 1)[0] ?? "");
  if (
    line === undefined ||
    [/<[A-Za-z!?/]|\]\(|=/u, USERINFO, LABELLED, AUTH_SCHEME].some(
      (risky) => line.search(risky) !== -1,
    )
  )
    return undefined;
  const explanation = line
    .replace(URL_PARTS, "[link]")
    .replace(OPAQUE, "[redacted]")
    .replace(/\p{Cc}/gu, " ")
    .split(/\s+/u)
    .filter((word) => word.length > 0)
    .join(" ");
  return explanation.length === 0 ? undefined : bounded(explanation);
}

/**
 * Removes credential-bearing values while keeping text useful as a
 * diagnostic: URL user information, queries and fragments; labelled
 * credentials, assignments and authorization values; and long opaque runs.
 */
export function redactCredentials(text: string): string {
  return text
    .replace(
      URL_PARTS,
      (_, scheme: string, location: string, rest: string | undefined) =>
        `${scheme}${location}${rest === undefined ? "" : "?[redacted]"}`,
    )
    .replace(AUTH_SCHEME, "$1[redacted]")
    .replace(LABELLED, "$1[redacted]")
    .replace(ASSIGNMENT, "$1[redacted]")
    .replace(OPAQUE, "[redacted]");
}

/** A diagnostic for the runtime log: the redacted stack or message, never the error object and its request data. */
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

/** An error body's human part where providers conventionally put it, and nowhere deeper. */
function bodyMessage(body: unknown): string | undefined {
  if (typeof body !== "object" || body === null) return undefined;
  if ("message" in body) return typeof body.message === "string" ? body.message : undefined;
  if (!("error" in body)) return undefined;
  const { error } = body;
  if (typeof error === "string") return error;
  if (typeof error === "object" && error !== null && "message" in error)
    return typeof error.message === "string" ? error.message : undefined;
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
