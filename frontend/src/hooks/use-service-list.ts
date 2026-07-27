import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";

/** Fetches the service names known to the supervisor, for the log window's service picker. */
export const useServiceList = (): string[] => {
  const [services, setServices] = useState<string[]>([]);

  useEffect(() => {
    void invoke<string[]>("list_services").then(setServices);
  }, []);

  return services;
};
