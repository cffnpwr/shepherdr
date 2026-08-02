import { describe, expect, it } from "bun:test";

import type { TailEvent } from "@/lib/tail.ts";

import { applyTailEvent, INITIAL_TAIL_LINES_STATE, MAX_DISPLAYED_LINES } from "@/lib/tail.ts";

/** Builds a `reset` event carrying `lines`. */
const reset = (lines: string[]): TailEvent => ({ event: "reset", data: { lines } });

/** Builds an `append` event carrying `lines`. */
const append = (lines: string[]): TailEvent => ({ event: "append", data: { lines } });

/** Builds an `error` event carrying `message`. */
const errorEvent = (message: string): TailEvent => ({ event: "error", data: { message } });

/** `n` distinct line texts starting at `offset`, e.g. `texts(2, 3) -> ["line 3", "line 4"]`. */
const texts = (n: number, offset = 0): string[] => (
  Array.from({ length: n }, (_unused, i) => `line ${offset + i}`)
);

describe("applyTailEvent", () => {
  it("[positive] reset replaces whatever was previously displayed", () => {
    const afterAppend = applyTailEvent(INITIAL_TAIL_LINES_STATE, append(texts(3)));

    const result = applyTailEvent(afterAppend, reset(["fresh a", "fresh b"]));

    expect(result.lines.map((line) => line.text)).toEqual(["fresh a", "fresh b"]);
  });

  it("[positive] append adds lines after whatever was previously displayed", () => {
    const afterFirst = applyTailEvent(INITIAL_TAIL_LINES_STATE, append(["a", "b"]));

    const result = applyTailEvent(afterFirst, append(["c"]));

    expect(result.lines.map((line) => line.text)).toEqual(["a", "b", "c"]);
  });

  it("[positive] exceeding MAX_DISPLAYED_LINES on append drops the oldest lines first", () => {
    const filled = applyTailEvent(INITIAL_TAIL_LINES_STATE, reset(texts(MAX_DISPLAYED_LINES)));

    const result = applyTailEvent(filled, append(["one more"]));

    expect(result.lines).toHaveLength(MAX_DISPLAYED_LINES);
    expect(result.lines[0]?.text).toBe("line 1");
    expect(result.lines.at(-1)?.text).toBe("one more");
  });

  it("[positive] a reset carrying more than MAX_DISPLAYED_LINES lines is itself capped", () => {
    const result = applyTailEvent(
      INITIAL_TAIL_LINES_STATE,
      reset(texts(MAX_DISPLAYED_LINES + 10)),
    );

    expect(result.lines).toHaveLength(MAX_DISPLAYED_LINES);
    expect(result.lines[0]?.text).toBe("line 10");
    expect(result.lines.at(-1)?.text).toBe(`line ${MAX_DISPLAYED_LINES + 9}`);
  });

  it("[positive] a dropped line's id is never reused by a later line", () => {
    const filled = applyTailEvent(INITIAL_TAIL_LINES_STATE, reset(texts(MAX_DISPLAYED_LINES)));
    const droppedId = filled.lines[0]?.id;
    expect(droppedId).toBeDefined();

    const afterAppends = texts(5, MAX_DISPLAYED_LINES).reduce(
      (state, line) => applyTailEvent(state, append([line])),
      filled,
    );

    expect(afterAppends.lines.some((line) => line.id === droppedId)).toBe(false);
    expect(afterAppends.lines.every((line) => line.id > (droppedId as number))).toBe(true);
  });

  it("[positive] ids increase monotonically across a reset rather than restarting at 0", () => {
    const afterFirst = applyTailEvent(INITIAL_TAIL_LINES_STATE, append(["a", "b"]));
    const lastIdBeforeReset = afterFirst.lines.at(-1)?.id;
    expect(lastIdBeforeReset).toBeDefined();

    const afterReset = applyTailEvent(afterFirst, reset(["c", "d"]));

    expect(afterReset.lines[0]?.id).toBeGreaterThan(lastIdBeforeReset as number);
    expect(afterReset.lines[1]?.id).toBeGreaterThan(afterReset.lines[0]?.id as number);
  });

  it("[negative] an error event leaves the displayed lines unchanged", () => {
    const before = applyTailEvent(INITIAL_TAIL_LINES_STATE, append(["a", "b"]));

    const result = applyTailEvent(before, errorEvent("something went wrong"));

    expect(result).toEqual(before);
  });
});
