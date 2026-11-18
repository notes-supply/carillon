import { bearer, reading, streamPath, type View } from "./requests.ts";
import { retryPause } from "./retry.ts";
import { streamMessages, type Received } from "./stream.ts";

type Ending = Received["kind"] | "nothing";
type Bytes = Uint8Array<ArrayBuffer>;

const KEEP_ALIVE_MS = 10_000;
const SILENT_AFTER_MS = 3 * KEEP_ALIVE_MS;
const SILENT = "silent";

function heardOn(stream: AbortController): () => void {
  const giveUp = (): void => stream.abort(SILENT);
  let timer = setTimeout(giveUp, SILENT_AFTER_MS);
  stream.signal.addEventListener("abort", () => clearTimeout(timer));
  return () => {
    clearTimeout(timer);
    if (!stream.signal.aborted) timer = setTimeout(giveUp, SILENT_AFTER_MS);
  };
}

function hearing(heard: () => void): TransformStream<Bytes, Bytes> {
  return new TransformStream({
    transform(bytes, controller) {
      heard();
      controller.enqueue(bytes);
    },
  });
}

function nextTask(): Promise<void> {
  return new Promise((resolve) => {
    const { port1, port2 } = new MessageChannel();
    port1.addEventListener("message", () => {
      port1.close();
      resolve();
    });
    port1.start();
    port2.postMessage(null);
  });
}

async function* attempt(
  view: View,
  token: string,
  signal: AbortSignal,
  heard: () => void,
): AsyncGenerator<Received, void, void> {
  const response = await fetch(streamPath(view), {
    headers: { authorization: bearer(token) },
    signal,
  });
  heard();
  if (reading(response.status) === "signed_out") yield { kind: "signed_out" };
  else if (response.ok && response.body !== null)
    yield* streamMessages(response.body.pipeThrough(hearing(heard)));
}

export async function* watch(
  view: View,
  token: string,
  signal: AbortSignal,
): AsyncGenerator<Received, void, void> {
  let failures = 0;
  while (!signal.aborted) {
    const stream = new AbortController();
    const either = AbortSignal.any([signal, stream.signal]);
    let ending: Ending = "nothing";
    try {
      for await (const message of attempt(view, token, either, heardOn(stream))) {
        ending = message.kind;
        if (ending === "snapshot") failures = 0;
        await nextTask();
        yield message;
        if (ending !== "snapshot") break;
      }
    } catch {
      if (stream.signal.reason !== SILENT) ending = "nothing";
    } finally {
      stream.abort();
    }
    if (signal.aborted || ending === "signed_out") return;
    if (ending === "snapshot") continue;
    if (ending === "nothing") {
      await nextTask();
      yield { kind: "unavailable" };
    }
    failures += 1;
    await retryPause(failures, signal);
  }
}
