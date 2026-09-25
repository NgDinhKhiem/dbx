import { describe, expect, it } from "vitest";
import { createSourceRowItemCache, createVisibleRowProjector } from "@/lib/dataGrid/dataGridRowItemCache";

type Ref = { id: number; displayIndex: number; sourceIndex: number; isDeleted: boolean; status: string };
type Item = Ref & { data: unknown[]; isDirtyCol: boolean[] };

const ref = (sourceIndex: number, overrides: Partial<Ref> = {}): Ref => ({ id: sourceIndex, displayIndex: sourceIndex, sourceIndex, isDeleted: false, status: "clean", ...overrides });

describe("createSourceRowItemCache", () => {
  it("reuses an item while the ref fields and the source row are unchanged", () => {
    const cache = createSourceRowItemCache<Ref, Item>();
    const rows = [[1], [2]];
    const clean: boolean[] = [false];
    cache.sync(rows, 0, clean);
    const item: Item = { ...ref(0), data: rows[0]!, isDirtyCol: clean };
    cache.set(ref(0), rows[0], item);

    cache.sync(rows, 0, clean);
    // A fresh but equal ref object still hits.
    expect(cache.get(ref(0), rows[0])).toBe(item);
    expect(cache.get(ref(0, { displayIndex: 3 }), rows[0])).toBeUndefined();
    expect(cache.get(ref(0, { isDeleted: true, status: "deleted" }), rows[0])).toBeUndefined();
    expect(cache.get(ref(0), [1])).toBeUndefined();
    expect(cache.get(ref(1), rows[1])).toBeUndefined();
  });

  it("starts a new generation when rows, the resolution version or the column flags change", () => {
    const cache = createSourceRowItemCache<Ref, Item>();
    const rows = [[1]];
    const clean: boolean[] = [false];
    const item: Item = { ...ref(0), data: rows[0]!, isDirtyCol: clean };
    const prime = () => {
      cache.sync(rows, 0, clean);
      cache.set(ref(0), rows[0], item);
    };

    prime();
    cache.sync([...rows], 0, clean);
    expect(cache.get(ref(0), rows[0])).toBeUndefined();

    prime();
    cache.sync(rows, 1, clean);
    expect(cache.get(ref(0), rows[0])).toBeUndefined();

    prime();
    cache.sync(rows, 0, [false]);
    expect(cache.get(ref(0), rows[0])).toBeUndefined();
  });

  it("drops entries for rows that became dirty", () => {
    const cache = createSourceRowItemCache<Ref, Item>();
    const rows = [[1]];
    cache.sync(rows, 0, []);
    cache.set(ref(0), rows[0], { ...ref(0), data: rows[0]!, isDirtyCol: [] });
    cache.delete(0);
    expect(cache.get(ref(0), rows[0])).toBeUndefined();
    expect(cache.size).toBe(0);
  });
});

describe("createVisibleRowProjector", () => {
  it("projects visible columns once per item and visible-column array", () => {
    const project = createVisibleRowProjector<Item>();
    const item: Item = { ...ref(0), data: ["a", "b", "c"], isDirtyCol: [false, true] };
    const visible = [2, 1];

    const projected = project(item, visible);
    expect(projected).toMatchObject({ id: 0, data: ["c", "b"], isDirtyCol: [false, true] });
    expect(project(item, visible)).toBe(projected);

    const other = project(item, [0]);
    expect(other).not.toBe(projected);
    expect(other.data).toEqual(["a"]);
    expect(project({ ...item }, [0])).not.toBe(other);
  });
});
