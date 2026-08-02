import type { ReactNode } from "react";

import { memo, useEffect, useMemo, useRef } from "react";
import { match } from "ts-pattern";

import type { AnsiColor, AnsiSegment } from "@/lib/ansi.ts";
import type { DisplayedLine } from "@/lib/tail.ts";

import { useTail } from "@/hooks/use-tail.ts";
import { parseAnsiLine } from "@/lib/ansi.ts";

/**
 * What `App` currently knows about the toolbar's service picker.
 *
 * A discriminated union rather than `string | null | undefined`: `??`/`?.` treat `null` and
 * `undefined` alike, so a stray nullish operator anywhere downstream could silently collapse
 * "not read yet" into "no service" without a type error. Branching on `kind` instead makes that
 * collapse a compile error -- `LogPane` matches on it with `ts-pattern`'s `.exhaustive()`, and a
 * variant it does not handle (an unhandled existing one, or a new variant nobody taught it about)
 * is a direct compile error naming the missing case, not a side effect of an unrelated tsconfig
 * flag.
 */
export type ServiceSelection = | { readonly kind: "pending"; }
  | { readonly kind: "unselected"; }
  | { readonly kind: "selected"; readonly name: string; };

/**
 * Converts the `string | null` shape the IPC boundary uses -- `selected_service`'s return value
 * and `service-selected`'s payload alike (see `crates/shepherdr-app/src/logs.rs` and
 * `crates/shepherdr-app/src/toolbar.rs`) -- into a {@link ServiceSelection}. The only place a raw
 * `string | null` becomes one, so nothing downstream has its own opportunity to reintroduce the
 * null/undefined mixing the union exists to rule out.
 */
export const toSelection = (value: string | null): ServiceSelection => (
  value === null ? { kind: "unselected" } : { kind: "selected", name: value }
);

/** How close to the bottom (in pixels) counts as "at the bottom" for auto-scroll purposes. */
const AUTO_SCROLL_THRESHOLD_PX = 32;

/** Centered placeholder shared by the "no service" and "tail failed with nothing read yet" states. */
const EmptyState = ({ children }: { children: string; }) => (
  <div className="
    flex h-full items-center justify-center
    bg-[color:var(--apple-text-background-color)]
    text-[length:var(--apple-type-body-size)]
    text-[color:var(--apple-secondary-label-color)]
  "
  >
    {children}
  </div>
);

/** Renders an {@link AnsiColor} as a CSS color value: a theme-following var, or a fixed RGB triplet. */
const colorToCss = (color: AnsiColor): string => (
  color.kind === "var" ? `var(${color.cssVar})` : `rgb(${color.r} ${color.g} ${color.b})`
);

/**
 * Renders one log line's already-parsed SGR segments. Every segment gets its own inline `style`
 * -- colors are resolved per line at parse time (256-color and 24-bit RGB are arbitrary values,
 * not a fixed set a Tailwind class could name), so a static class cannot express them.
 */
const renderSegments = (segments: readonly AnsiSegment[]): ReactNode => (
  segments.map((segment, index) => (
    <span
      // A segment's identity is fully determined by its position within the already-resolved
      // `segments` array, so an index key is safe.
      // eslint-disable-next-line @eslint-react/no-array-index-key
      key={index}
      style={{
        color: colorToCss(segment.foreground),
        backgroundColor: colorToCss(segment.background),
        fontWeight: segment.bold ? "bold" : undefined,
        textDecoration: segment.underline ? "underline" : undefined,
      }}
    >
      {segment.text}
    </span>
  ))
);

/**
 * One displayed row, memoized and keyed by {@link DisplayedLine.id} in {@link TailedLog} instead
 * of by array position: in the steady state, an append shifts every remaining line's index while
 * leaving its id and text untouched, so an id key (and the shallow prop comparison memo does
 * against `text`/`isLast`) is what lets an unchanged line skip both re-parsing (`parseAnsiLine`
 * runs in this component's body, so memo bailing out skips it too) and re-rendering.
 */
const LogLine = memo(({ text, isLast }: { text: string; isLast: boolean; }) => (
  <>
    {renderSegments(parseAnsiLine(text))}
    {isLast ? null : "\n"}
  </>
));

/**
 * The tailing half of {@link LogPane}, split out so that {@link LogPane}'s `"pending"` and
 * `"unselected"` branches never call {@link useTail} -- React does not allow a hook to be called
 * conditionally within one component.
 */
const TailedLog = ({ serviceName }: { serviceName: string; }) => {
  const { lines, error } = useTail(serviceName);
  const logRef = useRef<HTMLDivElement>(null);
  const followRef = useRef(true);

  // A tail failure with nothing read yet reads as the empty state; once there are lines on
  // screen, the failure instead reads as one appended line so the earlier output stays visible.
  // Its id continues from the last displayed line's so it never collides with an id `useTail` has
  // already handed out; the `?? -1` fallback is unreachable (this branch only runs when
  // `lines.length > 0`) but required by `noUncheckedIndexedAccess`.
  const displayLines = useMemo(
    (): readonly DisplayedLine[] => (error !== null && lines.length > 0
      ? [
        ...lines,
        { id: (lines[lines.length - 1]?.id ?? -1) + 1, text: `--- ログ取得が停止しました: ${error} ---` },
      ]
      : lines),
    [lines, error],
  );

  useEffect(() => {
    const container = logRef.current;
    if (container && followRef.current) {
      container.scrollTop = container.scrollHeight;
    }
  }, [displayLines]);

  const handleScroll = () => {
    const container = logRef.current;
    if (!container) {
      return;
    }
    const distanceFromBottom = container.scrollHeight - container.scrollTop - container.clientHeight;
    followRef.current = distanceFromBottom < AUTO_SCROLL_THRESHOLD_PX;
  };

  if (error !== null && lines.length === 0) {
    return <EmptyState>ログを取得できませんでした</EmptyState>;
  }

  return (
    <div
      ref={logRef}
      onScroll={handleScroll}
      className="
        h-full overflow-auto bg-[color:var(--apple-text-background-color)] px-3
        py-2 font-mono text-[length:var(--apple-type-body-size)]
        leading-[var(--apple-type-body-line-height)] whitespace-pre-wrap
      "
    >
      {displayLines.map((line, index) => (
        <LogLine key={line.id} text={line.text} isLast={index === displayLines.length - 1} />
      ))}
    </div>
  );
};

/**
 * Shows the log of whatever `selection` names, or an empty state.
 *
 * `{ kind: "pending" }` (the initial selection has not been read back from `App` yet) and
 * `{ kind: "unselected" }` (the configuration defines no service at all) both show no lines, but
 * only the latter is worth telling the user about: showing the "no service" message during the
 * former would flash a wrong claim whenever a service actually is configured, so pending instead
 * renders only the log area's own background.
 */
export const LogPane = ({ selection }: { selection: ServiceSelection; }) => (
  match(selection)
    .with({ kind: "pending" }, () => (
      <div className="h-full bg-[color:var(--apple-text-background-color)]" />
    ))
    .with({ kind: "unselected" }, () => <EmptyState>サービスが設定されていません</EmptyState>)
    .with({ kind: "selected" }, ({ name }) => (
      // Keyed so that switching services remounts `TailedLog`, starting its displayed lines and
      // scroll position fresh (see `useTail`).
      <TailedLog key={name} serviceName={name} />
    ))
    .exhaustive()
);
