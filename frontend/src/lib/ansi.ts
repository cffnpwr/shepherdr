/**
 * ANSI escape sequence handling for log lines.
 *
 * Log lines arrive as plain strings (see `TailEvent` in `@/lib/tail.ts`); a service run with
 * `FORCE_COLOR` or `--color=always` writes raw SGR (`ESC [ ... m`) sequences into them, since
 * output is captured through a plain pipe rather than a PTY (see
 * `crates/shepherdr-core/src/spawn.rs`). This module turns such a line into styled segments a
 * renderer can draw directly, and strips every other escape sequence (cursor movement, line
 * erase, ...) so no control bytes ever reach the screen.
 *
 * Kept free of DOM/React so it can be unit tested in isolation; `@/components/log-pane.tsx` is
 * the only caller.
 */

import { match, P } from "ts-pattern";

const ESC = "\u001b";
const BEL = "\u0007";

/** A resolved, renderer-ready color: either a theme-following CSS custom property or a fixed RGB triplet. */
export type AnsiColor = | { readonly kind: "var"; readonly cssVar: string; }
  | { readonly kind: "rgb"; readonly r: number; readonly g: number; readonly b: number; };

/** One run of text sharing the same resolved style. */
export type AnsiSegment = {
  readonly text: string;
  readonly foreground: AnsiColor;
  readonly background: AnsiColor;
  readonly bold: boolean;
  readonly underline: boolean;
};

/**
 * The 16 SGR named colors, indexed 0 (black) through 7 (white). Also used for the "bright"
 * variants (SGR 90-97 / 100-107): macOS system colors have no light/dark pair for a single color,
 * so bright and normal intentionally share the same mapping. Backed by `--ansi-*` in
 * `index.css`, which in turn follows the theme through `--apple-*`.
 */
const NAMED_COLORS: readonly AnsiColor[] = [
  { kind: "var", cssVar: "--ansi-black" },
  { kind: "var", cssVar: "--ansi-red" },
  { kind: "var", cssVar: "--ansi-green" },
  { kind: "var", cssVar: "--ansi-yellow" },
  { kind: "var", cssVar: "--ansi-blue" },
  { kind: "var", cssVar: "--ansi-magenta" },
  { kind: "var", cssVar: "--ansi-cyan" },
  { kind: "var", cssVar: "--ansi-white" },
];

const DEFAULT_FOREGROUND: AnsiColor = { kind: "var", cssVar: "--apple-text-color" };
const DEFAULT_BACKGROUND: AnsiColor = { kind: "var", cssVar: "--apple-text-background-color" };

/**
 * Resolves an SGR-indexed named color (0-7), falling back to the default foreground for an
 * out-of-range index. The fallback is unreachable in practice -- every caller derives `index`
 * from an SGR code already constrained to 0-7 -- but `noUncheckedIndexedAccess` cannot see that,
 * and this is cheaper than asserting it away.
 */
const namedColor = (index: number): AnsiColor => NAMED_COLORS[index] ?? DEFAULT_FOREGROUND;

/** Mutable SGR state accumulated while scanning one line; `null` means "not set, use the default". */
type SgrState = {
  foreground: AnsiColor | null;
  background: AnsiColor | null;
  bold: boolean;
  underline: boolean;
  reverse: boolean;
};

const initialSgrState = (): SgrState => ({
  foreground: null,
  background: null,
  bold: false,
  underline: false,
  reverse: false,
});

/**
 * Resolves `state` into the pair of colors a segment should actually be drawn with: the default
 * color fills in whichever side was never set, and reverse video (`7`) swaps foreground and
 * background *after* that fallback -- matching a real terminal, where reverse video still has two
 * colors to exchange even when neither was explicitly set.
 */
const resolveColors = (state: SgrState): { foreground: AnsiColor; background: AnsiColor; } => {
  const foreground = state.foreground ?? DEFAULT_FOREGROUND;
  const background = state.background ?? DEFAULT_BACKGROUND;
  return state.reverse
    ? { foreground: background, background: foreground }
    : { foreground, background };
};

const clampByte = (value: number): number => {
  if (Number.isNaN(value)) {
    return 0;
  }
  return Math.min(255, Math.max(0, Math.trunc(value)));
};

/** xterm's 6x6x6 color cube level for one 0-5 axis value (256-color indices 16-231). */
const cubeLevel = (value: number): number => (value === 0 ? 0 : (40 * value) + 55);

/** Resolves one 256-color palette index (`38;5;n` / `48;5;n`) to a color. */
const indexedColor = (index: number): AnsiColor => {
  const n = clampByte(index);
  if (n < 16) {
    return namedColor(n % 8);
  }
  if (n < 232) {
    const cube = n - 16;
    return {
      kind: "rgb",
      r: cubeLevel(Math.floor(cube / 36)),
      g: cubeLevel(Math.floor((cube % 36) / 6)),
      b: cubeLevel(cube % 6),
    };
  }
  const gray = 8 + ((n - 232) * 10);
  return { kind: "rgb", r: gray, g: gray, b: gray };
};

/**
 * Parses one SGR parameter token into a number, treating an empty token (from `ESC[m` or a stray
 * `;;`) as `0` per the "omitted parameter defaults to 0" convention, and anything non-numeric as
 * `NaN` for the caller to ignore rather than throw on.
 */
const parseParam = (token: string): number => (token === "" ? 0 : Number(token));

/**
 * Applies one `38;...` / `48;...` extended color selector starting at `params[start]` (the mode:
 * `5` for a 256-color index, `2` for 24-bit RGB). Returns the resolved color (`null` for an
 * unrecognized mode) and how many parameters -- including the mode itself -- it consumed, so the
 * caller can advance past it even when some expected values are missing: a missing value reads as
 * `undefined` past the array's end, never throwing, and is treated as `0`.
 */
const readExtendedColor = (
  params: readonly number[],
  start: number,
): { color: AnsiColor | null; consumed: number; } => {
  const mode = params[start];
  if (mode === 5) {
    return { color: indexedColor(params[start + 1] ?? NaN), consumed: 2 };
  }
  if (mode === 2) {
    return {
      color: {
        kind: "rgb",
        r: clampByte(params[start + 1] ?? NaN),
        g: clampByte(params[start + 2] ?? NaN),
        b: clampByte(params[start + 3] ?? NaN),
      },
      consumed: 4,
    };
  }
  return { color: null, consumed: 1 };
};

/** Applies one SGR sequence's parameters (already split on `;`) to `state` in place. */
const applySgr = (paramsStr: string, state: SgrState): void => {
  const params = paramsStr.split(";").map(parseParam);
  let i = 0;
  while (i < params.length) {
    const code = params[i];
    if (code === undefined || Number.isNaN(code)) {
      i += 1;
      continue;
    }

    if (code === 38 || code === 48) {
      const { color, consumed } = readExtendedColor(params, i + 1);
      if (color !== null) {
        if (code === 38) {
          state.foreground = color;
        } else {
          state.background = color;
        }
      }
      i += consumed + 1;
      continue;
    }

    match(code)
      .with(0, () => Object.assign(state, initialSgrState()))
      .with(1, () => {
        state.bold = true;
      })
      .with(4, () => {
        state.underline = true;
      })
      .with(7, () => {
        state.reverse = true;
      })
      .with(22, () => {
        state.bold = false;
      })
      .with(24, () => {
        state.underline = false;
      })
      .with(27, () => {
        state.reverse = false;
      })
      .with(P.number.between(30, 37), () => {
        state.foreground = namedColor(code - 30);
      })
      .with(39, () => {
        state.foreground = null;
      })
      .with(P.number.between(40, 47), () => {
        state.background = namedColor(code - 40);
      })
      .with(49, () => {
        state.background = null;
      })
      .with(P.number.between(90, 97), () => {
        state.foreground = namedColor(code - 90);
      })
      .with(P.number.between(100, 107), () => {
        state.background = namedColor(code - 100);
      })
      // Any other code (italic, strikethrough, an out-of-range number, ...) is left unhandled --
      // exactly the desired "unsupported, ignored" behavior.
      .otherwise(() => {});
    i += 1;
  }
};

const isCsiParamOrIntermediate = (ch: string): boolean => {
  const code = ch.charCodeAt(0);
  return code >= 0x20 && code <= 0x3f;
};

const isCsiFinalByte = (ch: string): boolean => {
  const code = ch.charCodeAt(0);
  return code >= 0x40 && code <= 0x7e;
};

/**
 * Parses one log line's SGR sequences into styled segments, discarding every other escape
 * sequence (cursor movement, line erase, OSC, ...) along with the raw control bytes themselves.
 *
 * SGR state does not carry across lines: each call starts from the default style. Log producers
 * that colorize output overwhelmingly reset (or fully re-specify) color per line; per-line
 * scoping keeps this a pure, stateless mapping from one line to its segments, instead of
 * requiring the caller to thread state through `useTail`'s line buffer for a case that in
 * practice does not arise.
 *
 * Malformed input never throws. An unrecognized SGR parameter is ignored; an unterminated escape
 * sequence (`ESC[` with no final byte, or a lone trailing `ESC`) drops the rest of the line from
 * the point it starts, since nothing after an unterminated sequence can be trusted to be plain
 * text.
 */
export const parseAnsiLine = (line: string): AnsiSegment[] => {
  const segments: AnsiSegment[] = [];
  const state = initialSgrState();
  let buffer = "";

  const flush = (): void => {
    if (buffer.length === 0) {
      return;
    }
    const { foreground, background } = resolveColors(state);
    segments.push({
      text: buffer,
      foreground,
      background,
      bold: state.bold,
      underline: state.underline,
    });
    buffer = "";
  };

  let i = 0;
  while (i < line.length) {
    const ch = line[i];
    if (ch === undefined) {
      break;
    }
    if (ch !== ESC) {
      buffer += ch;
      i += 1;
      continue;
    }

    const next = line[i + 1];
    if (next === "[") {
      let j = i + 2;
      let scanned = line[j];
      while (scanned !== undefined && isCsiParamOrIntermediate(scanned)) {
        j += 1;
        scanned = line[j];
      }
      const finalByte = line[j];
      if (finalByte === undefined || !isCsiFinalByte(finalByte)) {
        break;
      }
      if (finalByte === "m") {
        flush();
        applySgr(line.slice(i + 2, j), state);
      }
      i = j + 1;
      continue;
    }

    if (next === "]") {
      // OSC sequence, terminated by BEL or ST (`ESC \`).
      let j = i + 2;
      let terminated = false;
      while (j < line.length) {
        const c = line[j];
        if (c === BEL) {
          j += 1;
          terminated = true;
          break;
        }
        if (c === ESC && line[j + 1] === "\\") {
          j += 2;
          terminated = true;
          break;
        }
        j += 1;
      }
      if (!terminated) {
        break;
      }
      i = j;
      continue;
    }

    if (next === undefined) {
      break;
    }

    // Any other two-byte escape (e.g. `ESC M`): not SGR, drop just the two bytes.
    i += 2;
  }
  flush();

  return segments;
};
