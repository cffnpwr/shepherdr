import { describe, expect, it } from "bun:test";

import type { AnsiColor, AnsiSegment } from "@/lib/ansi.ts";

import { parseAnsiLine } from "@/lib/ansi.ts";

/** Spelled out via `String.fromCharCode` rather than a `` escape so it reads unambiguously in a diff. */
const ESC = String.fromCharCode(0x1b);

/** Builds a CSI sequence, e.g. `csi("31", "m")` for SGR "set foreground to red". */
const csi = (params: string, final: string): string => `${ESC}[${params}${final}`;

const DEFAULT_FOREGROUND: AnsiColor = { kind: "var", cssVar: "--apple-text-color" };
const DEFAULT_BACKGROUND: AnsiColor = { kind: "var", cssVar: "--apple-text-background-color" };

/** Builds the expected segment for plain, unstyled text -- the baseline most assertions diff against. */
const plain = (text: string): AnsiSegment => ({
  text,
  foreground: DEFAULT_FOREGROUND,
  background: DEFAULT_BACKGROUND,
  bold: false,
  underline: false,
});

describe("parseAnsiLine", () => {
  it("[positive] plain text yields a single segment with the default style", () => {
    const result = parseAnsiLine("hello");
    expect(result).toEqual([plain("hello")]);
  });

  it("[positive] a basic 16-color foreground code maps to the matching --ansi-* color", () => {
    const result = parseAnsiLine(`${csi("31", "m")}red${csi("0", "m")}`);
    expect(result).toEqual([
      { ...plain("red"), foreground: { kind: "var", cssVar: "--ansi-red" } },
    ]);
  });

  it("[positive] a basic 16-color background code maps to the matching --ansi-* color", () => {
    const result = parseAnsiLine(`${csi("42", "m")}greenbg${csi("0", "m")}`);
    expect(result).toEqual([
      { ...plain("greenbg"), background: { kind: "var", cssVar: "--ansi-green" } },
    ]);
  });

  it("[positive] black maps to the default text color (--apple-text-color)", () => {
    const result = parseAnsiLine(`${csi("30", "m")}black${csi("0", "m")}`);
    expect(result).toEqual([
      { ...plain("black"), foreground: { kind: "var", cssVar: "--ansi-black" } },
    ]);
  });

  it("[positive] white maps to the background-like color (--apple-text-background-color)", () => {
    const result = parseAnsiLine(`${csi("37", "m")}white${csi("0", "m")}`);
    expect(result).toEqual([
      { ...plain("white"), foreground: { kind: "var", cssVar: "--ansi-white" } },
    ]);
  });

  it("[positive] a bright color code resolves to the same var as its normal counterpart", () => {
    const result = parseAnsiLine(`${csi("91", "m")}brightred${csi("0", "m")}`);
    expect(result).toEqual([
      { ...plain("brightred"), foreground: { kind: "var", cssVar: "--ansi-red" } },
    ]);
  });

  it("[positive] bold sets bold: true", () => {
    const result = parseAnsiLine(`${csi("1", "m")}bold${csi("0", "m")}`);
    expect(result).toEqual([{ ...plain("bold"), bold: true }]);
  });

  it("[positive] underline sets underline: true", () => {
    const result = parseAnsiLine(`${csi("4", "m")}underline${csi("0", "m")}`);
    expect(result).toEqual([{ ...plain("underline"), underline: true }]);
  });

  it("[positive] reverse swaps foreground and background when both are still the defaults", () => {
    const result = parseAnsiLine(`${csi("7", "m")}reversed${csi("0", "m")}`);
    expect(result).toEqual([
      { ...plain("reversed"), foreground: DEFAULT_BACKGROUND, background: DEFAULT_FOREGROUND },
    ]);
  });

  it("[positive] reverse swaps explicitly set foreground and background colors", () => {
    const result = parseAnsiLine(`${csi("31;42;7", "m")}swapped${csi("0", "m")}`);
    expect(result).toEqual([
      {
        ...plain("swapped"),
        foreground: { kind: "var", cssVar: "--ansi-green" },
        background: { kind: "var", cssVar: "--ansi-red" },
      },
    ]);
  });

  it("[positive] 22 clears bold", () => {
    const result = parseAnsiLine(`${csi("1", "m")}bold${csi("22", "m")}plain`);
    expect(result).toEqual([
      { ...plain("bold"), bold: true },
      plain("plain"),
    ]);
  });

  it("[positive] 24 clears underline", () => {
    const result = parseAnsiLine(`${csi("4", "m")}underline${csi("24", "m")}plain`);
    expect(result).toEqual([
      { ...plain("underline"), underline: true },
      plain("plain"),
    ]);
  });

  it("[positive] 27 clears reverse", () => {
    const result = parseAnsiLine(`${csi("7", "m")}reversed${csi("27", "m")}plain`);
    expect(result).toEqual([
      { ...plain("reversed"), foreground: DEFAULT_BACKGROUND, background: DEFAULT_FOREGROUND },
      plain("plain"),
    ]);
  });

  it("[positive] 0 resets every attribute", () => {
    const result = parseAnsiLine(`${csi("1;4;31;42", "m")}styled${csi("0", "m")}plain`);
    expect(result).toEqual([
      {
        text: "styled",
        foreground: { kind: "var", cssVar: "--ansi-red" },
        background: { kind: "var", cssVar: "--ansi-green" },
        bold: true,
        underline: true,
      },
      plain("plain"),
    ]);
  });

  it("[positive] ESC[m with an omitted parameter is treated as a full reset", () => {
    const result = parseAnsiLine(`${csi("1", "m")}bold${csi("", "m")}plain`);
    expect(result).toEqual([
      { ...plain("bold"), bold: true },
      plain("plain"),
    ]);
  });

  it("[positive] a chained multi-parameter code (1;31;42) applies all of its parameters at once", () => {
    const result = parseAnsiLine(`${csi("1;31;42", "m")}styled${csi("0", "m")}`);
    expect(result).toEqual([
      {
        text: "styled",
        foreground: { kind: "var", cssVar: "--ansi-red" },
        background: { kind: "var", cssVar: "--ansi-green" },
        bold: true,
        underline: false,
      },
    ]);
  });

  it("[positive] 256-color palette indices 0-15 map to the basic 16-color --ansi-* vars", () => {
    const result = parseAnsiLine(`${csi("38;5;9", "m")}fg${csi("0", "m")}`);
    expect(result).toEqual([
      { ...plain("fg"), foreground: { kind: "var", cssVar: "--ansi-red" } },
    ]);
  });

  it("[positive] a 256-color palette index in the 6x6x6 cube converts to RGB", () => {
    // 38;5;196 -> cube index (196-16)=180 -> r=5,g=0,b=0 -> r=40*5+55=255, g=0, b=0
    const result = parseAnsiLine(`${csi("38;5;196", "m")}fg${csi("0", "m")}`);
    expect(result).toEqual([
      { ...plain("fg"), foreground: { kind: "rgb", r: 255, g: 0, b: 0 } },
    ]);
  });

  it("[positive] a 256-color palette index in the grayscale ramp converts to RGB", () => {
    // 38;5;232 -> gray = 8 + (232-232)*10 = 8
    const result = parseAnsiLine(`${csi("38;5;232", "m")}fg${csi("0", "m")}`);
    expect(result).toEqual([
      { ...plain("fg"), foreground: { kind: "rgb", r: 8, g: 8, b: 8 } },
    ]);
  });

  it("[positive] a 24-bit color (38;2;r;g;b) becomes RGB using the given values as-is", () => {
    const result = parseAnsiLine(
      `${csi("38;2;12;34;56", "m")}fg${csi("48;2;200;150;100", "m")}bg${csi("0", "m")}`,
    );
    expect(result).toEqual([
      { ...plain("fg"), foreground: { kind: "rgb", r: 12, g: 34, b: 56 } },
      {
        ...plain("bg"),
        foreground: { kind: "rgb", r: 12, g: 34, b: 56 },
        background: { kind: "rgb", r: 200, g: 150, b: 100 },
      },
    ]);
  });

  it("[positive] a cursor-movement sequence (ESC[2A) is stripped and leaves nothing in the output", () => {
    const result = parseAnsiLine(`before${csi("2", "A")}after`);
    expect(result).toEqual([plain("beforeafter")]);
  });

  it("[positive] a line-erase sequence (ESC[2K) is stripped and leaves nothing in the output", () => {
    const result = parseAnsiLine(`before${csi("2", "K")}after`);
    expect(result).toEqual([plain("beforeafter")]);
  });

  it("[positive] a multi-parameter CSI sequence such as cursor positioning (ESC[1;1H) is also stripped", () => {
    const result = parseAnsiLine(`before${csi("1;1", "H")}after`);
    expect(result).toEqual([plain("beforeafter")]);
  });

  it("[negative] an unterminated ESC[ does not throw, and only the text before it is shown", () => {
    const line = `before${ESC}[31`;
    expect(() => parseAnsiLine(line)).not.toThrow();
    expect(parseAnsiLine(line)).toEqual([plain("before")]);
  });

  it("[negative] a lone trailing ESC does not throw, and only the text before it is shown", () => {
    const line = `before${ESC}`;
    expect(() => parseAnsiLine(line)).not.toThrow();
    expect(parseAnsiLine(line)).toEqual([plain("before")]);
  });

  it("[negative] an unknown SGR parameter number (999) does not throw and is ignored", () => {
    const line = `${csi("999", "m")}text${csi("0", "m")}`;
    expect(() => parseAnsiLine(line)).not.toThrow();
    expect(parseAnsiLine(line)).toEqual([plain("text")]);
  });

  it("[negative] an out-of-range 256-color index (38;5;999) does not throw and is clamped to 0-255", () => {
    const line = `${csi("38;5;999", "m")}text${csi("0", "m")}`;
    expect(() => parseAnsiLine(line)).not.toThrow();
    // clamp(999) -> 255 -> grayscale: 8 + (255-232)*10 = 238
    expect(parseAnsiLine(line)).toEqual([
      { ...plain("text"), foreground: { kind: "rgb", r: 238, g: 238, b: 238 } },
    ]);
  });

  it("[negative] out-of-range 24-bit color components (38;2;300;-10;5) do not throw and are clamped to 0-255", () => {
    const line = `${csi("38;2;300;-10;5", "m")}text${csi("0", "m")}`;
    expect(() => parseAnsiLine(line)).not.toThrow();
    expect(parseAnsiLine(line)).toEqual([
      { ...plain("text"), foreground: { kind: "rgb", r: 255, g: 0, b: 5 } },
    ]);
  });

  it("[negative] a non-numeric SGR parameter (ESC[<m) does not throw and is ignored", () => {
    // "<" is a valid CSI parameter byte (0x3C) but not a digit, so it reaches "m" as a
    // genuinely non-numeric SGR parameter -- unlike a letter, which would itself terminate
    // the CSI sequence as an (unrecognized) final byte before "m" is ever reached.
    const line = `${csi("<", "m")}text`;
    expect(() => parseAnsiLine(line)).not.toThrow();
    expect(parseAnsiLine(line)).toEqual([plain("text")]);
  });

  it("[negative] an empty string does not throw and yields an empty segment array", () => {
    expect(() => parseAnsiLine("")).not.toThrow();
    expect(parseAnsiLine("")).toEqual([]);
  });
});
