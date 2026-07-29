import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";

import type { ServiceSelection } from "@/components/log-pane.tsx";

import { LogPane, toSelection } from "@/components/log-pane.tsx";

/** The event the log window's native toolbar (`crate::toolbar`) emits on every picker change. */
const SELECTION_EVENT = "service-selected";

const App = () => {
  // `{ kind: "pending" }` until `selected_service` resolves: the selection may already be
  // settled by the time this component starts, and treating that gap as `{ kind: "unselected" }`
  // (see `LogPane`) would flash the wrong empty state whenever a service actually is configured.
  const [selection, setSelection] = useState<ServiceSelection>({ kind: "pending" });

  useEffect(() => {
    let cancelled = false;
    let unlistenFn: (() => void) | undefined;
    // Guards the reverse race: an event landing while `selected_service`'s response is still in
    // flight must win over that response once it arrives.
    let receivedEvent = false;

    const setup = async () => {
      // `listen`'s own `plugin:event|listen` IPC call has to resolve before the listener is
      // actually registered (see node_modules/@tauri-apps/api/event.js). Awaiting that before
      // calling `selected_service` is what closes the gap between the two IPC round trips: an
      // emit landing there would otherwise reach neither the not-yet-registered listener nor
      // `selected_service`, which would still answer with the pre-emit value.
      const unlisten = await listen<string | null>(SELECTION_EVENT, (event) => {
        receivedEvent = true;
        setSelection(toSelection(event.payload));
      });

      if (cancelled) {
        // Cleanup already ran while `listen` was still resolving, so nothing stored `unlisten`
        // for it to call; unregister right here instead, so nothing leaks.
        unlisten();
        return;
      }
      unlistenFn = unlisten;

      const initial = await invoke<string | null>("selected_service");
      // TypeScript narrows `cancelled` to `false` from the `if (cancelled)` check above and does
      // not account for the cleanup closure reassigning it while this `await` was suspended, so
      // it reads this check as always true; it is not -- cleanup can run during that suspension.
      // eslint-disable-next-line @typescript-eslint/no-unnecessary-condition -- see comment above
      if (!cancelled && !receivedEvent) {
        setSelection(toSelection(initial));
      }
    };

    void setup();

    return () => {
      cancelled = true;
      unlistenFn?.();
    };
  }, []);

  return (
    <main className="h-screen">
      <LogPane selection={selection} />
    </main>
  );
};

export default App;
