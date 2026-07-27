import { useEffect, useRef } from "react";

import { useTail } from "@/hooks/use-tail.ts";

/** How close to the bottom (in pixels) counts as "at the bottom" for auto-scroll purposes. */
const AUTO_SCROLL_THRESHOLD_PX = 32;

/**
 * Tails and displays `serviceName`'s log, auto-scrolling to the bottom as it grows unless the
 * user has scrolled up to read earlier output.
 *
 * Mount this behind a `key={serviceName}` (see {@link useTail}) so that switching services starts
 * both the displayed lines and the scroll position fresh.
 */
export const LogPane = ({ serviceName }: { serviceName: string; }) => {
  const { lines, error } = useTail(serviceName);
  const logRef = useRef<HTMLDivElement>(null);
  const followRef = useRef(true);

  useEffect(() => {
    const container = logRef.current;
    if (container && followRef.current) {
      container.scrollTop = container.scrollHeight;
    }
  }, [lines]);

  const handleScroll = () => {
    const container = logRef.current;
    if (!container) {
      return;
    }
    const distanceFromBottom = container.scrollHeight - container.scrollTop - container.clientHeight;
    followRef.current = distanceFromBottom < AUTO_SCROLL_THRESHOLD_PX;
  };

  return (
    <>
      {error !== null && (
        <p className="
          rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive
        "
        >
          {error}
        </p>
      )}
      <div
        ref={logRef}
        onScroll={handleScroll}
        className="
          flex-1 overflow-auto rounded-md border bg-muted/30 p-3 font-mono
          text-xs whitespace-pre-wrap
        "
      >
        {lines.join("\n")}
      </div>
    </>
  );
};
