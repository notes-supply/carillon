import { ORIGIN } from "./origin.ts";
import { bearer, reading, streamPath, type View } from "./requests.ts";
import { retryPause } from "./retry.ts";
import { streamMessages, type Received } from "./stream.ts";

type Ending = Received["kind"] | "nothing";

async function* attempt(
  view: View,
  token: string,
  signal: AbortSignal,
): AsyncGenerator<Received, void, void> {
  const response = await fetch(ORIGIN + streamPath(view), {
    headers: { authorization: bearer(token) },
    signal,
  });
  if (reading(response.status) === "signed_out") yield { kind: "signed_out" };
  else if (response.ok && response.body !== null) yield* streamMessages(response.body);
}

export async function* watch(
  view: View,
  token: string,
  signal: AbortSignal,
): AsyncGenerator<Received, void, void> {
  let failures = 0;
  while (!signal.aborted) {
    const stream = new AbortController();
    let ending: Ending = "nothing";
    try {
      for await (const message of attempt(view, token, AbortSignal.any([signal, stream.signal]))) {
        ending = message.kind;
        if (ending === "snapshot") failures = 0;
        yield message;
        if (ending !== "snapshot") break;
      }
    } catch {
      ending = "nothing";
    } finally {
      stream.abort();
    }
    if (signal.aborted || ending === "signed_out") return;
    if (ending === "snapshot") continue;
    if (ending === "nothing") yield { kind: "unavailable" };
    failures += 1;
    await retryPause(failures, signal);
  }
}
