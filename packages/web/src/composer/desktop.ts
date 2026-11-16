export function onDesktop(): boolean {
  return matchMedia("(min-width: 46rem)").matches;
}
