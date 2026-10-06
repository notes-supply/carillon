/** Runs operations one at a time, in arrival order. */
export class Serial {
  #queue: Promise<void> = Promise.resolve();

  async run<T>(operation: () => Promise<T>): Promise<T> {
    const prior = this.#queue;
    let release = (): void => undefined;
    this.#queue = new Promise<void>((resolveQueue) => {
      release = resolveQueue;
    });
    await prior;
    try {
      return await operation();
    } finally {
      release();
    }
  }
}
