/** Counts work a page redirect would destroy, such as a running upload. */
let running = 0;

export function beginActivity(): () => void {
  running++;
  let ended = false;
  return () => {
    if (!ended) {
      ended = true;
      running--;
    }
  };
}

export function isBusy(): boolean {
  return running > 0;
}
