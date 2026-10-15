import * as api from "@/lib/backend/api";
import { createDriverPrewarmer, STARTUP_PREWARM_DELAY_MS, type DriverPrewarmer } from "@/lib/connection/driverPrewarm";
import { useConnectionStore } from "@/stores/connectionStore";
import { useSettingsStore } from "@/stores/settingsStore";

let prewarmer: DriverPrewarmer | null = null;

function sharedPrewarmer(): DriverPrewarmer {
  if (prewarmer) return prewarmer;
  const connectionStore = useConnectionStore();
  const settingsStore = useSettingsStore();
  prewarmer = createDriverPrewarmer({
    prewarm: (connectionId) => api.mqPrewarmAgent(connectionId),
    isEnabled: () => settingsStore.editorSettings.prewarmDrivers !== false,
    getConfig: (connectionId) => connectionStore.getConfig(connectionId),
    isConnected: (connectionId) => connectionStore.connectedIds.has(connectionId),
  });
  return prewarmer;
}

/** Hover / expand intent on a sidebar connection: warm its driver runtime. */
export function requestDriverPrewarm(connectionId: string | undefined): void {
  if (!connectionId) return;
  sharedPrewarmer().request(connectionId);
}

/** Warm driver runtimes for saved connections shortly after startup. */
export function scheduleStartupDriverPrewarm(delayMs = STARTUP_PREWARM_DELAY_MS): () => void {
  const timer = setTimeout(() => {
    const connectionStore = useConnectionStore();
    sharedPrewarmer().requestStartup(connectionStore.connections);
  }, delayMs);
  return () => clearTimeout(timer);
}
