/**
 * Runs `worker` over `items` with at most `limit` in flight at once. Stops starting new
 * items after the first failure (or once `signal` aborts) and rethrows that failure
 * once in-flight items settle. Workers that must keep going past individual failures
 * (e.g. bulk delete) should catch inside `worker` instead.
 */
export async function runPool<T>(
  items: readonly T[],
  limit: number,
  worker: (item: T, index: number) => Promise<void>,
  signal?: AbortSignal,
): Promise<void> {
  let next = 0;
  let failed = false;
  let firstError: unknown;

  async function lane() {
    while (next < items.length && !failed && !signal?.aborted) {
      const index = next++;
      try {
        await worker(items[index], index);
      } catch (err) {
        if (!failed) {
          failed = true;
          firstError = err;
        }
      }
    }
  }

  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, lane));
  if (failed) throw firstError;
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
