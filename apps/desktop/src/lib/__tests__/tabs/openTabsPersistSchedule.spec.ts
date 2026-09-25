import { describe, expect, it } from "vitest";
import { OPEN_TABS_PERSIST_DEBOUNCE_MS, openTabsPersistDebounceMs } from "@/lib/tabs/openTabsPersistSchedule";

describe("openTabsPersistDebounceMs", () => {
  it("keeps the short debounce for ordinary workspaces", () => {
    expect(openTabsPersistDebounceMs([])).toBe(OPEN_TABS_PERSIST_DEBOUNCE_MS);
    expect(openTabsPersistDebounceMs([{ sql: "select 1" }, { sql: "x".repeat(1000), originalSql: "y" }])).toBe(300);
  });

  it("spaces out writes while large scripts are open", () => {
    expect(openTabsPersistDebounceMs([{ sql: "x".repeat(200 * 1024) }, { sql: "", originalSql: "y".repeat(100 * 1024) }])).toBe(1000);
    expect(openTabsPersistDebounceMs([{ sql: "x".repeat(3 * 1024 * 1024) }])).toBe(2000);
  });
});
