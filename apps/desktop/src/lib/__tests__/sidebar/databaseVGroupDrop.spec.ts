// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from "vitest";
import type { TreeNode } from "@/types/database";
import { resolveDatabaseVGroupDropTarget } from "@/lib/sidebar/sidebarTableVGroupDrag";

function row(nodeId: string): HTMLElement {
  const element = document.createElement("div");
  element.setAttribute("data-node-id", nodeId);
  const label = document.createElement("span");
  element.append(label);
  document.body.append(element);
  return label;
}

/** Pretend the pointer is over these elements, topmost first. */
function pointerOver(...elements: Element[]) {
  Object.defineProperty(document, "elementsFromPoint", { configurable: true, value: vi.fn(() => elements) });
}

const databaseGroup: TreeNode = { id: "table-vgroup:dev", label: "Dev", type: "table-vgroup", connectionId: "pg", vgroupId: "dev", vgroupKind: "databases", children: [] };
const tableGroup: TreeNode = { id: "table-vgroup:orders", label: "Orders", type: "table-vgroup", connectionId: "pg", database: "app", vgroupId: "orders", vgroupKind: "tables", children: [] };
const database: TreeNode = { id: "pg:app", label: "app", type: "database", connectionId: "pg", database: "app", children: [tableGroup] };
const connection: TreeNode = { id: "pg", label: "PG", type: "connection", connectionId: "pg", children: [databaseGroup, database] };
const otherConnection: TreeNode = { id: "mysql", label: "MySQL", type: "connection", connectionId: "mysql", children: [] };
const tree = [connection, otherConnection];

describe("resolveDatabaseVGroupDropTarget", () => {
  afterEach(() => {
    document.body.innerHTML = "";
    Reflect.deleteProperty(document, "elementsFromPoint");
  });

  it("drops into a database group of the same connection", () => {
    pointerOver(row(databaseGroup.id));
    expect(resolveDatabaseVGroupDropTarget(0, 0, tree, "pg")).toEqual({ node: databaseGroup, groupId: "dev" });
  });

  it("takes rows out of their group when released on their connection", () => {
    pointerOver(row(connection.id));
    expect(resolveDatabaseVGroupDropTarget(0, 0, tree, "pg")).toEqual({ node: connection, groupId: null });
  });

  it("ignores table groups, other rows and other connections", () => {
    pointerOver(row(tableGroup.id), row(database.id), row(otherConnection.id));
    expect(resolveDatabaseVGroupDropTarget(0, 0, tree, "pg")).toBeNull();
  });

  it("looks through overlays stacked above the row", () => {
    const chip = document.createElement("div");
    document.body.append(chip);
    pointerOver(chip, row(databaseGroup.id));
    expect(resolveDatabaseVGroupDropTarget(0, 0, tree, "pg")?.groupId).toBe("dev");
  });
});
