import { describe, expect, it, vi } from "vitest";
import { allowUnsignedPluginsThisSession, clearLegacyUnsignedPluginOptIn, LEGACY_PLUGIN_ALLOW_UNSIGNED_STORAGE_KEY } from "@/lib/plugins/pluginUnsignedSession";

describe("unsigned plugin session opt-in", () => {
  it("starts disabled on every app start", () => {
    expect(allowUnsignedPluginsThisSession.value).toBe(false);
  });

  it("drops the opt-in older builds persisted forever", () => {
    const removeItem = vi.fn();
    clearLegacyUnsignedPluginOptIn({ removeItem });
    expect(removeItem).toHaveBeenCalledWith(LEGACY_PLUGIN_ALLOW_UNSIGNED_STORAGE_KEY);
  });

  it("tolerates unavailable storage", () => {
    expect(() =>
      clearLegacyUnsignedPluginOptIn({
        removeItem: () => {
          throw new Error("denied");
        },
      }),
    ).not.toThrow();
  });
});
