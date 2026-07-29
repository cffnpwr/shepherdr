import { useEffect, useMemo, useRef } from "react";

import { useTail } from "@/hooks/use-tail.ts";

/**
 * What `App` currently knows about the toolbar's service picker.
 *
 * A discriminated union rather than `string | null | undefined`: `??`/`?.` treat `null` and
 * `undefined` alike, so a stray nullish operator anywhere downstream could silently collapse
 * "not read yet" into "no service" without a type error. Branching on `kind` instead makes that
 * collapse a compile error -- `LogPane`'s switch has to be exhaustive, and a case it does not
 * handle (an unhandled existing one, or a new variant nobody taught it about) leaves a code path
 * with no return, which `noImplicitReturns` (see tsconfig) rejects.
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
  const displayLines = useMemo(
    () => (error !== null && lines.length > 0
      ? [...lines, `--- ログ取得が停止しました: ${error} ---`]
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
        text-[color:var(--apple-text-color)]
      "
    >
      {displayLines.join("\n")}
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
export const LogPane = ({ selection }: { selection: ServiceSelection; }) => {
  switch (selection.kind) {
    case "pending":
      return (
        <div className="h-full bg-[color:var(--apple-text-background-color)]" />
      );
    case "unselected":
      return <EmptyState>サービスが設定されていません</EmptyState>;
    case "selected":
      // Keyed so that switching services remounts `TailedLog`, starting its displayed lines and
      // scroll position fresh (see `useTail`).
      return <TailedLog key={selection.name} serviceName={selection.name} />;
  }
};
