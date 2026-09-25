import type { SavedOpenTab } from "@/lib/app/openTabsPersistence";

export const OPEN_TABS_PERSIST_DEBOUNCE_MS = 300;

/**
 * Debounce for the open-tabs autosave. The backend stores the whole payload on
 * every save, so while typing in multi-megabyte scripts writes are spaced out
 * further; small workspaces keep the original 300 ms.
 */
export function openTabsPersistDebounceMs(savedTabs: readonly Pick<SavedOpenTab, "sql" | "originalSql">[]): number {
  let chars = 0;
  for (const tab of savedTabs) chars += (tab.sql?.length ?? 0) + (tab.originalSql?.length ?? 0);
  if (chars >= 2 * 1024 * 1024) return 2000;
  if (chars >= 256 * 1024) return 1000;
  return OPEN_TABS_PERSIST_DEBOUNCE_MS;
}
