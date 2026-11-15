import { createSubscriber } from "svelte/reactivity";

const everySecond = createSubscriber((update) => {
  const timer = setInterval(update, 1000);
  return () => clearInterval(timer);
});

function now(): number {
  everySecond();
  return Math.floor(Date.now() / 1000);
}

const today = $derived(new Date(now() * 1000).setHours(0, 0, 0, 0) / 1000);

export const clock = {
  get now(): number {
    return now();
  },
  get today(): number {
    return today;
  },
};
