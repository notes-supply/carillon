let queue: Promise<void> = Promise.resolve();

export async function serialize<T>(operation: () => Promise<T>, signal?: AbortSignal): Promise<T> {
  const prior = queue;
  let release = (): void => undefined;
  queue = new Promise<void>((resolveQueue) => {
    release = resolveQueue;
  });
  await prior;
  try {
    signal?.throwIfAborted();
    return await operation();
  } finally {
    release();
  }
}
