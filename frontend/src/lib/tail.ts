/**
 * The event shape delivered over the `tail_log` command's channel, mirroring
 * `shepherdr_app_lib::logs::tail::TailEvent` (`#[serde(tag = "event", content = "data")]`).
 */
export type TailEvent = | { event: "reset"; data: { lines: string[]; }; }
  | { event: "append"; data: { lines: string[]; }; }
  | { event: "error"; data: { message: string; }; };

/**
 * How many of the most recent lines the log window keeps displayed.
 *
 * Bounds the DOM node count and memory the window holds regardless of how long it has been
 * tailing, without needing scrollback beyond what a human is realistically going to read.
 */
export const MAX_DISPLAYED_LINES = 5000;

/** Keeps only the most recent {@link MAX_DISPLAYED_LINES} entries of `lines`. */
const capLines = (lines: string[]): string[] => {
  if (lines.length <= MAX_DISPLAYED_LINES) {
    return lines;
  }
  return lines.slice(lines.length - MAX_DISPLAYED_LINES);
};

/**
 * Applies one `TailEvent` to the currently displayed lines, capping the result at
 * {@link MAX_DISPLAYED_LINES}.
 */
export const applyTailEvent = (lines: string[], event: TailEvent): string[] => {
  switch (event.event) {
    case "reset":
      return capLines(event.data.lines);
    case "append":
      return capLines([...lines, ...event.data.lines]);
    case "error":
      return lines;
  }
};
