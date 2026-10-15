import { describe, expect, it } from "vitest";
import type { TreeNode } from "@/types/database";
import {
  applyTableVGroupsToChildren,
  clearTableVGroupPlacement,
  collectTableTreeNames,
  createTableVGroup,
  emptyTableVGroupLayout,
  isDatabaseVGroupRow,
  moveTableToVGroup,
  normalizeTableVGroupLayout,
  resolveTableVGroupScopeFromNode,
  setTableVGroupRule,
  stripTableVGroupsFromChildren,
  tableVGroupCandidateNames,
  tableVGroupHasExplicitPlacement,
  tableVGroupKindOfContainerNode,
  tableVGroupPatternError,
  tableVGroupPatternMatches,
  tableVGroupRowName,
  tableVGroupScopeKey,
  type TableVGroupLayout,
  type TableVGroupScope,
} from "@/lib/table/tableVGroup";

const DB_SCOPE: TableVGroupScope = { connectionId: "conn-1", objectType: "databases" };

function databaseNode(name: string, label = name): TreeNode {
  return { id: `conn-1:${name}`, label, type: "database", connectionId: "conn-1", database: name, children: [] };
}

function tableNode(name: string): TreeNode {
  return { id: `table:${name}`, label: name, type: "table", connectionId: "conn-1", database: "db1" };
}

function connectionNode(children: TreeNode[]): TreeNode {
  return { id: "conn-1", label: "Conn", type: "connection", connectionId: "conn-1", children };
}

/** Group label -> member labels, plus the ungrouped rows, as plain data. */
function outline(nodes: TreeNode[]): Array<string | Record<string, unknown>> {
  return nodes.map((node) => (node.type === "table-vgroup" ? { [node.label]: outline(node.children ?? []) } : node.label));
}

function ruleLayout(...rules: Array<[name: string, pattern: string]>): TableVGroupLayout {
  let layout = emptyTableVGroupLayout();
  for (const [name, pattern] of rules) layout = createTableVGroup(layout, name, null, { pattern }).layout;
  return layout;
}

describe("database virtual groups", () => {
  it("treats a connection as the container of database groups with its own scope key", () => {
    expect(tableVGroupKindOfContainerNode({ type: "connection" })).toBe("databases");
    expect(tableVGroupScopeKey(DB_SCOPE)).toBe("conn-1\u0000\u0000\u0000\u0000\u0000databases");
    expect(tableVGroupScopeKey({ objectType: "databases" })).toBeNull();
    // Tables scopes are unchanged.
    expect(tableVGroupScopeKey({ connectionId: "conn-1", database: "db1" })).toBe("conn-1\u0000\u0000\u0000db1\u0000");
  });

  it("resolves a plain databases scope to the connection without losing its kind", () => {
    const tree = [connectionNode([databaseNode("app_dev")])];
    expect(resolveTableVGroupScopeFromNode(tree, DB_SCOPE)).toMatchObject({ connectionId: "conn-1", objectType: "databases" });
    expect(resolveTableVGroupScopeFromNode(tree, tree[0]!)).toMatchObject({ connectionId: "conn-1", objectType: "databases" });
  });

  it("groups databases by their real name, not the display label", () => {
    const shortened = databaseNode("projects/p/instances/i/databases/orders", "orders");
    expect(tableVGroupRowName(shortened)).toBe("projects/p/instances/i/databases/orders");
    const layout = moveTableToVGroup(createTableVGroup(emptyTableVGroupLayout(), "Spanner", null).layout, "projects/p/instances/i/databases/orders", null, "database");
    const groupId = layout.groups[0]!.id;
    const moved = moveTableToVGroup(layout, "projects/p/instances/i/databases/orders", groupId, "database");
    expect(outline(applyTableVGroupsToChildren([shortened, databaseNode("other")], moved, DB_SCOPE))).toEqual([{ Spanner: ["orders"] }, "other"]);
  });

  it("detects database rows directly under their connection, including grouped ones", () => {
    const grouped = databaseNode("app_dev");
    const plain = databaseNode("app_qa");
    const nested = { ...databaseNode("inner"), id: "conn-1:app_qa:inner" };
    const group: TreeNode = { id: "table-vgroup:g", label: "G", type: "table-vgroup", connectionId: "conn-1", vgroupId: "g", children: [grouped] };
    const tree: TreeNode[] = [{ id: "grp", label: "Folder", type: "connection-group", children: [connectionNode([group, { ...plain, children: [nested] }])] }];
    expect(isDatabaseVGroupRow(tree, grouped)).toBe(true);
    expect(isDatabaseVGroupRow(tree, plain)).toBe(true);
    expect(isDatabaseVGroupRow(tree, nested)).toBe(false);
    expect(isDatabaseVGroupRow(tree, tableNode("t"))).toBe(false);
  });

  it("lists candidate names of a container without the projected groups", () => {
    const layout = ruleLayout(["Dev", "_dev$"]);
    const container = connectionNode(applyTableVGroupsToChildren([databaseNode("app_dev"), databaseNode("app_qa")], layout, DB_SCOPE));
    expect(tableVGroupCandidateNames(container)).toEqual(["app_dev", "app_qa"]);
    expect([...collectTableTreeNames(container.children!, "databases")].sort()).toEqual(["app_dev", "app_qa"]);
  });
});

describe("regex rule groups", () => {
  const databases = () => ["app_dev", "billing_dev", "app_qa", "billing_qa", "PERF_main", "postgres"].map((name) => databaseNode(name));

  it("collects matching rows into rule groups, first matching rule wins", () => {
    const layout = ruleLayout(["Dev", "_dev$"], ["QA", "_qa$"], ["Apps", "^app_"]);
    expect(outline(applyTableVGroupsToChildren(databases(), layout, DB_SCOPE))).toEqual([{ Dev: ["app_dev", "billing_dev"] }, { QA: ["app_qa", "billing_qa"] }, { Apps: [] }, "PERF_main", "postgres"]);
  });

  it("matches case-insensitively by default and case-sensitively when asked", () => {
    const insensitive = ruleLayout(["Perf", "^perf_"]);
    expect(outline(applyTableVGroupsToChildren(databases(), insensitive, DB_SCOPE))[0]).toEqual({ Perf: ["PERF_main"] });
    const sensitive = createTableVGroup(emptyTableVGroupLayout(), "Perf", null, { pattern: "^perf_", ignoreCase: false }).layout;
    expect(outline(applyTableVGroupsToChildren(databases(), sensitive, DB_SCOPE))[0]).toEqual({ Perf: [] });
  });

  it("keeps manual placements ahead of rules", () => {
    let layout = ruleLayout(["Dev", "_dev$"]);
    layout = createTableVGroup(layout, "Billing", null).layout;
    const billing = layout.groups.find((group) => group.name === "Billing")!.id;
    layout = moveTableToVGroup(layout, "billing_dev", billing, "database");
    // "Remove from group" pins a row at the top level, out of every rule.
    layout = moveTableToVGroup(layout, "app_dev", null, "database");
    expect(outline(applyTableVGroupsToChildren(databases(), layout, DB_SCOPE))).toEqual([{ Dev: [] }, { Billing: ["billing_dev"] }, "app_dev", "app_qa", "billing_qa", "PERF_main", "postgres"]);
    expect(tableVGroupHasExplicitPlacement(layout, "app_dev", "database")).toBe(true);

    // Clearing the placement hands the row back to the rules.
    const cleared = clearTableVGroupPlacement(layout, "app_dev", "database");
    expect(tableVGroupHasExplicitPlacement(cleared, "app_dev", "database")).toBe(false);
    expect(outline(applyTableVGroupsToChildren(databases(), cleared, DB_SCOPE))[0]).toEqual({ Dev: ["app_dev"] });
  });

  it("applies rules on nested subgroups", () => {
    let layout = createTableVGroup(emptyTableVGroupLayout(), "Env", null).layout;
    const env = layout.groups[0]!.id;
    layout = createTableVGroup(layout, "QA", env, { pattern: "_qa$" }).layout;
    expect(outline(applyTableVGroupsToChildren(databases(), layout, DB_SCOPE))[0]).toEqual({ Env: [{ QA: ["app_qa", "billing_qa"] }] });
  });

  it("ignores invalid patterns instead of breaking the sidebar", () => {
    const layout = ruleLayout(["Broken", "(unclosed"]);
    expect(outline(applyTableVGroupsToChildren(databases(), layout, DB_SCOPE))).toEqual([{ Broken: [] }, "app_dev", "billing_dev", "app_qa", "billing_qa", "PERF_main", "postgres"]);
  });

  it("re-projects idempotently and strips back to the flat list", () => {
    const layout = ruleLayout(["Dev", "_dev$"]);
    const once = applyTableVGroupsToChildren(databases(), layout, DB_SCOPE);
    const twice = applyTableVGroupsToChildren(once, layout, DB_SCOPE);
    expect(outline(twice)).toEqual(outline(once));
    expect(
      stripTableVGroupsFromChildren(twice)
        .map((node) => node.label)
        .sort(),
    ).toEqual(
      databases()
        .map((node) => node.label)
        .sort(),
    );
  });

  it("works for table groups too", () => {
    const layout = ruleLayout(["Orders", "^order"]);
    const tables = ["order_items", "orders", "customers"].map(tableNode);
    expect(outline(applyTableVGroupsToChildren(tables, layout, { connectionId: "conn-1", database: "db1" }))).toEqual([{ Orders: ["order_items", "orders"] }, "customers"]);
  });

  it("sets, replaces and clears a group's rule", () => {
    const created = createTableVGroup(emptyTableVGroupLayout(), "Dev", null);
    const withRule = setTableVGroupRule(created.layout, created.groupId, { pattern: "_dev$", ignoreCase: false });
    expect(withRule.groups[0]).toMatchObject({ pattern: "_dev$", patternIgnoreCase: false });
    const cleared = setTableVGroupRule(withRule, created.groupId, null);
    expect(cleared.groups[0]).toEqual({ id: created.groupId, name: "Dev", collapsed: false });
  });

  it("keeps rules through normalization and drops invalid ones", () => {
    const layout = normalizeTableVGroupLayout({
      groups: [
        { id: "a", name: "Dev", collapsed: false, pattern: "_dev$" },
        { id: "b", name: "Big", collapsed: false, pattern: "x".repeat(501) },
        { id: "c", name: "Odd", collapsed: false, pattern: 42 },
      ],
      order: [
        { type: "group", id: "a" },
        { type: "group", id: "b" },
        { type: "group", id: "c" },
      ],
    });
    expect(layout.groups).toEqual([
      { id: "a", name: "Dev", collapsed: false, pattern: "_dev$", patternIgnoreCase: true },
      { id: "b", name: "Big", collapsed: false },
      { id: "c", name: "Odd", collapsed: false },
    ]);
  });

  it("validates patterns and previews matches", () => {
    expect(tableVGroupPatternError("_dev$")).toBeNull();
    expect(tableVGroupPatternError("   ")).toBe("empty");
    expect(tableVGroupPatternError("x".repeat(501))).toBe("too_long");
    expect(tableVGroupPatternError("(unclosed")).toMatch(/.+/);
    expect(tableVGroupPatternMatches(["app_dev", "APP_QA", "x"], "^app_")).toEqual(["app_dev", "APP_QA"]);
    expect(tableVGroupPatternMatches(["app_dev", "APP_QA"], "^app_", false)).toEqual(["app_dev"]);
    expect(tableVGroupPatternMatches(["a"], "(")).toEqual([]);
  });
});
