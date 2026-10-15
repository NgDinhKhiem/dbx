import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { toRaw } from "vue";
import type { ConnectionConfig, TreeNode } from "@/types/database";

/** Group label -> member labels, plus the ungrouped rows, as plain data. */
function outline(nodes: TreeNode[]): Array<string | Record<string, unknown>> {
  return nodes.map((node) => (node.type === "table-vgroup" ? { [node.label]: outline(node.children ?? []) } : node.label));
}

describe("connectionStore database virtual groups", () => {
  beforeEach(() => {
    vi.resetModules();
    const storage = new Map<string, string>();
    vi.stubGlobal("localStorage", {
      getItem: vi.fn((key: string) => storage.get(key) ?? null),
      setItem: vi.fn((key: string, value: string) => storage.set(key, value)),
      removeItem: vi.fn((key: string) => storage.delete(key)),
    });
    vi.doMock("@/lib/backend/tauriRuntime", () => ({ isTauriRuntime: () => false }));
    vi.doMock("@/lib/backend/api", () => ({
      loadConnections: vi.fn().mockResolvedValue([]),
      loadEditorSettings: vi.fn().mockResolvedValue(null),
      checkConnectionHealth: vi.fn().mockResolvedValue(undefined),
      listInstalledAgents: vi.fn().mockResolvedValue([]),
      loadPinnedTreeNodeIds: vi.fn().mockResolvedValue([]),
      loadSidebarLayout: vi.fn().mockResolvedValue(null),
      loadTableVGroups: vi.fn().mockResolvedValue({}),
      saveTableVGroups: vi.fn().mockResolvedValue(undefined),
      loadTunnelProfiles: vi.fn().mockResolvedValue([]),
      loadSchemaCache: vi.fn().mockResolvedValue(null),
      saveSchemaCache: vi.fn().mockResolvedValue(undefined),
      deleteSchemaCachePrefix: vi.fn().mockResolvedValue(undefined),
      saveConnections: vi.fn().mockResolvedValue(undefined),
      saveEditorSettings: vi.fn().mockResolvedValue(undefined),
      savePinnedTreeNodeIds: vi.fn().mockResolvedValue(undefined),
    }));
    setActivePinia(createPinia());
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("groups a connection's databases by rule and by hand, and persists the layout", async () => {
    vi.useFakeTimers();
    const api = await import("@/lib/backend/api");
    const { tableVGroupScopeKey } = await import("@/lib/table/tableVGroup");
    const { useConnectionStore } = await import("@/stores/connectionStore");
    const config: ConnectionConfig = { id: "pg", name: "PG", db_type: "postgres", host: "127.0.0.1", port: 5432, username: "postgres", password: "", database: "postgres" };
    vi.mocked(api.loadConnections).mockResolvedValue([config]);
    const store = useConnectionStore();
    await store.initFromDisk();

    const connection = store.treeNodes[0]!;
    const expandedTable: TreeNode = { id: "pg:app_dev:table:t", label: "t", type: "table", connectionId: "pg", database: "app_dev" };
    const databases: TreeNode[] = ["app_dev", "app_qa", "billing_dev", "postgres"].map((name) => ({ id: `pg:${name}`, label: name, type: "database", connectionId: "pg", database: name, children: [] }));
    databases[0]!.isExpanded = true;
    databases[0]!.children = [expandedTable];
    connection.isExpanded = true;
    connection.children = databases;

    const scope = store.databaseVGroupScope("pg");
    expect(store.tableVGroupCandidateNamesFor(scope)).toEqual(["app_dev", "app_qa", "billing_dev", "postgres"]);

    const devGroup = store.createTableVGroup(scope, "Dev", null, { pattern: "_dev$" })!;
    expect(outline(connection.children)).toEqual([{ Dev: ["app_dev", "billing_dev"] }, "app_qa", "postgres"]);

    const qaGroup = store.createTableVGroup(scope, "QA", null)!;
    store.moveTableToVGroup(scope, "app_qa", qaGroup, "database");
    expect(outline(connection.children)).toEqual([{ Dev: ["app_dev", "billing_dev"] }, { QA: ["app_qa"] }, "postgres"]);

    // Grouped rows are the same node objects: expansion and loaded children survive.
    const grouped = connection.children.find((node) => node.vgroupId === devGroup)!.children![0]!;
    expect(toRaw(grouped)).toBe(databases[0]);
    expect(grouped.isExpanded).toBe(true);
    expect(grouped.children).toEqual([expandedTable]);

    store.setTableVGroupRule(scope, devGroup, null);
    expect(outline(connection.children)).toEqual([{ Dev: [] }, { QA: ["app_qa"] }, "app_dev", "billing_dev", "postgres"]);

    await vi.advanceTimersByTimeAsync(400);
    const key = tableVGroupScopeKey(scope)!;
    expect(api.saveTableVGroups).toHaveBeenLastCalledWith(key, expect.objectContaining({ groups: expect.arrayContaining([expect.objectContaining({ id: qaGroup, name: "QA" })]) }));
  });
});
