import { Channel, invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";

import type { DisplayedLine, TailEvent } from "@/lib/tail.ts";

import { applyTailEvent, INITIAL_TAIL_LINES_STATE } from "@/lib/tail.ts";

interface TailState {
  lines: readonly DisplayedLine[];
  error: string | null;
}

/**
 * Tails `serviceName`'s log file over the `tail_log` command's channel.
 *
 * Stops the backend's tail while the document is hidden (the window was hidden from the tray, not
 * closed -- see `crate::window`) and resumes it once visible again, so a log window left open in
 * the background does not keep polling a file no one is looking at.
 *
 * Callers should mount this behind a `key={serviceName}` so that switching services remounts it,
 * starting the displayed lines fresh, rather than reconciling the old and new tail's state here.
 */
export const useTail = (serviceName: string): TailState => {
  const [state, setState] = useState(INITIAL_TAIL_LINES_STATE);
  const [error, setError] = useState<string | null>(null);
  const [resumeToken, setResumeToken] = useState(0);

  useEffect(() => {
    const onVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        void invoke("stop_tail");
      } else {
        setResumeToken((token) => token + 1);
      }
    };
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, []);

  useEffect(() => {
    if (document.visibilityState === "hidden") {
      return;
    }

    const channel = new Channel<TailEvent>();
    channel.onmessage = (event) => {
      if (event.event === "error") {
        setError(event.data.message);
        return;
      }
      setState((current) => applyTailEvent(current, event));
    };

    void invoke("tail_log", { name: serviceName, onEvent: channel }).catch(
      (invokeError: unknown) => {
        setError(String(invokeError));
      },
    );

    return () => {
      void invoke("stop_tail");
    };
  }, [serviceName, resumeToken]);

  return { lines: state.lines, error };
};
