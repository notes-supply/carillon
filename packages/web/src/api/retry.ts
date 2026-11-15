import { retryDelayMs } from "./requests.ts";

export function retryPause(failures: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const waited = (): void => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", waited);
      resolve();
    };
    const timer = setTimeout(waited, retryDelayMs(failures));
    signal?.addEventListener("abort", waited);
  });
}
