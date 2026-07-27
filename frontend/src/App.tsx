import { useState } from "react";

import { LogPane } from "@/components/log-pane.tsx";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select.tsx";
import { useServiceList } from "@/hooks/use-service-list.ts";

const App = () => {
  const services = useServiceList();
  // No service has been explicitly picked yet: fall back to the first one once the list loads,
  // without an effect (https://react.dev/learn/you-might-not-need-an-effect).
  const [chosen, setChosen] = useState<string | null>(null);
  const selected = chosen ?? services[0] ?? null;

  return (
    <main className="flex h-screen flex-col gap-3 p-4">
      <header className="flex items-center gap-3">
        {/*
          Keep `value` as `string | null`, never `?? undefined`: Base UI's Select decides
          controlled vs. uncontrolled once, from whether `value` is `undefined` on the first
          render (@base-ui/react/select's useControlled), and does not revisit that decision.
          `undefined` here on the still-empty-services first render would permanently disconnect
          the trigger's displayed value from `selected`.
        */}
        <Select value={selected} onValueChange={setChosen}>
          <SelectTrigger>
            <SelectValue placeholder="サービスを選択" />
          </SelectTrigger>
          <SelectContent>
            {services.map((service) => (
              <SelectItem key={service} value={service}>
                {service}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </header>

      {selected !== null && <LogPane key={selected} serviceName={selected} />}
    </main>
  );
};

export default App;
