import { ref } from "vue";

/**
 * Key the "allow unsigned development packages" switch used to be persisted
 * under. It is only read to delete a stale opt-in left by older builds.
 */
export const LEGACY_PLUGIN_ALLOW_UNSIGNED_STORAGE_KEY = "dbx-plugin-allow-unsigned";

/**
 * Session-only opt-in for installing unsigned plugin packages. Deliberately
 * kept in memory: it resets on every app restart so a one-off development
 * install never leaves signature checks relaxed for good.
 */
export const allowUnsignedPluginsThisSession = ref(false);

export function clearLegacyUnsignedPluginOptIn(storage: Pick<Storage, "removeItem"> | undefined = safeLocalStorage()): void {
  try {
    storage?.removeItem(LEGACY_PLUGIN_ALLOW_UNSIGNED_STORAGE_KEY);
  } catch {
    // Storage unavailable (private mode); nothing was persisted there anyway.
  }
}

function safeLocalStorage(): Storage | undefined {
  try {
    return typeof localStorage === "undefined" ? undefined : localStorage;
  } catch {
    return undefined;
  }
}
