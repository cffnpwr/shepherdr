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
 * Render cost is proportional to the number of displayed lines, not to how many changed in one
 * update: with `log-pane.tsx`'s DOM shape (one `<span>` per ANSI segment), a steady-state
 * single-line append with 10 segments/line measured a 190ms median to reach the screen at 5000
 * displayed lines, against 40ms at 1000 lines. 1000 keeps that append well under what a frame can
 * absorb.
 */
export const MAX_DISPLAYED_LINES = 1000;

/**
 * One displayed line paired with a stable, monotonically-increasing id.
 *
 * `log-pane.tsx` keys each rendered row on `id` rather than on the line's position in the array,
 * so a row `React.memo` wraps can tell an unchanged line apart from one that merely shifted
 * position when older lines are capped off the front -- an index key changes for every remaining
 * line on every append, defeating memoization even though almost all of those lines' content
 * never changed.
 */
export type DisplayedLine = {
  readonly id: number;
  readonly text: string;
};

/**
 * {@link applyTailEvent}'s accumulated state: the currently displayed lines plus the next id to
 * hand out. `nextId` is tracked separately from `lines` (rather than derived from the last
 * displayed line's id) because capping -- or a `reset` to fewer lines than were displayed before
 * -- can leave `lines` empty or short of every id ever issued, and an id must still never be
 * reused once that happens.
 */
export type TailLinesState = {
  readonly lines: readonly DisplayedLine[];
  readonly nextId: number;
};

/** The state a fresh tail (a freshly mounted `useTail`) starts from. */
export const INITIAL_TAIL_LINES_STATE: TailLinesState = { lines: [], nextId: 0 };

/** Assigns consecutive ids starting at `nextId` to `texts`. */
const assignIds = (texts: readonly string[], nextId: number): DisplayedLine[] => (
  texts.map((text, index) => ({ id: nextId + index, text }))
);

/** Keeps only the most recent {@link MAX_DISPLAYED_LINES} entries of `lines`. */
const capLines = (lines: readonly DisplayedLine[]): readonly DisplayedLine[] => {
  if (lines.length <= MAX_DISPLAYED_LINES) {
    return lines;
  }
  return lines.slice(lines.length - MAX_DISPLAYED_LINES);
};

/**
 * Applies one `TailEvent` to `state`, capping the displayed lines at {@link MAX_DISPLAYED_LINES}.
 *
 * `reset` (the initial window, or a fresh generation after log rotation -- see
 * `crates/shepherdr-app/src/logs/tail.rs`) replaces the displayed lines but keeps issuing ids from
 * where `state` left off rather than restarting at 0: an id being unique for the lifetime of the
 * `useTail` instance is what lets it double as a position-independent React key, and a mid-session
 * generation change is not a reason to give that up.
 */
export const applyTailEvent = (state: TailLinesState, event: TailEvent): TailLinesState => {
  switch (event.event) {
    case "reset": {
      const lines = assignIds(event.data.lines, state.nextId);
      return { lines: capLines(lines), nextId: state.nextId + lines.length };
    }
    case "append": {
      const appended = assignIds(event.data.lines, state.nextId);
      return {
        lines: capLines([...state.lines, ...appended]),
        nextId: state.nextId + appended.length,
      };
    }
    case "error":
      return state;
  }
};
