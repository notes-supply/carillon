/** Runs operations one at a time, in arrival order. */
export class Serial {
  #queue: Promise<void> = Promise.resolve();

  /**
   * Runs `operation` once those before it have run. One whose `signal`
   * aborts while it waits never runs: it rejects with the signal's reason,
   * and the next waits only for those before it.
   */
  async run<T>(operation: () => Promise<T>, signal?: AbortSignal): Promise<T> {
    const prior = this.#queue;
    let release = (): void => undefined;
    this.#queue = new Promise<void>((resolveQueue) => {
      release = resolveQueue;
    });
    try {
      await (signal === undefined ? prior : Promise.race([prior, aborted(signal)]));
    } catch (reason) {
      void prior.then(release);
      throw reason;
    }
    try {
      return await operation();
    } finally {
      release();
    }
  }
}

function aborted(signal: AbortSignal): Promise<never> {
  return new Promise((_, reject) => {
    if (signal.aborted) reject(signal.reason);
    else signal.addEventListener("abort", () => reject(signal.reason), { once: true });
  });
}
