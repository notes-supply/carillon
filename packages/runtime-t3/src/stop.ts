/** How a person is told what T3 confirmed of a turn's stopped or blocked work. */

/** How a stopped or blocked turn's native work ended, as T3 confirmed it. */
export interface Stop {
  /** T3 reported no active run and no background work. */
  readonly quiet: boolean;
  /** A request of the turn is still open in T3. */
  readonly requestOpen: boolean;
}

/** The person's view of an unresolved background report, saying only what T3 confirmed. */
export function wakeReport(stop: Stop | undefined): string {
  return stop?.quiet === true
    ? "T3 confirmed that nothing of it is still running. Send another message to continue."
    : "T3 does not confirm that its native process has stopped, so a result may still arrive; the next turn interrupts any run it finds active. Send another message to continue.";
}

/**
 * A terminal run's explanation, followed by what T3 confirmed about the
 * background work it left behind when there was any to stop.
 */
export function withStop(explanation: string, stop: Stop | undefined): string {
  if (stop === undefined) return explanation;
  return `${explanation.replace(/\.$/u, "")}; ${
    stop.quiet
      ? "T3 confirmed that its background work was stopped."
      : "T3 has not confirmed that its background work stopped, so it may still be running; the next turn interrupts any run it finds active."
  }`;
}

/** The person's view of a stop, saying only what T3 confirmed. */
export function stopReport(stop: Stop | undefined): string {
  if (stop?.quiet !== true)
    return "T3 has not confirmed that the run stopped, so it may still be waiting. Send the answer in a new message.";
  if (stop.requestOpen)
    return "the run has ended but T3 still lists the request as open. Send the answer in a new message.";
  return "the run was stopped. Send the answer in a new message.";
}
