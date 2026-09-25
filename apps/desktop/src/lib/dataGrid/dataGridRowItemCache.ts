/**
 * Identity caches for DataGrid row items.
 *
 * `displayItems` is rebuilt whenever edit state changes (dirty rows, deleted
 * rows, quick-entry editing, large-value resolution). Rebuilding a fresh
 * RowItem for every one of 100k rows, and then a rows × visible-columns copy
 * of each, turned every single cell edit into O(rows · columns) allocations.
 * These caches keep the RowItem (and its visible-column projection) of rows
 * whose inputs did not change, so only the affected rows are recreated.
 */

export interface DataGridCachedRowShape {
  id: number;
  displayIndex: number;
  sourceIndex: number;
  isDeleted: boolean;
  status: string;
}

export interface DataGridRowItemShape {
  data: unknown[];
  isDirtyCol: boolean[];
}

interface SourceRowItemEntry<Ref extends DataGridCachedRowShape, Item> {
  ref: Ref;
  row: readonly unknown[];
  item: Item;
}

/**
 * Cache for RowItems of clean (unedited) source rows, keyed by source index.
 * An entry is reused only while the display ref fields and the source row
 * array are unchanged within the same generation. A generation ends when the
 * source rows array is replaced (reload, sort, page), when source cells are
 * mutated in place (the large-value resolution version bumps), or when the
 * shared clean dirty-column flags change (column count). Edited rows are never
 * cached because their per-row change maps are mutated in place.
 */
export function createSourceRowItemCache<Ref extends DataGridCachedRowShape, Item>() {
  let entries = new Map<number, SourceRowItemEntry<Ref, Item>>();
  let generationRows: unknown;
  let generationVersion: number | undefined;
  let generationColumns: unknown;

  return {
    sync(rows: unknown, version: number, cleanColumns: unknown) {
      if (generationRows === rows && generationVersion === version && generationColumns === cleanColumns) return;
      entries = new Map();
      generationRows = rows;
      generationVersion = version;
      generationColumns = cleanColumns;
    },
    get(ref: Ref, row: readonly unknown[] | undefined): Item | undefined {
      const entry = entries.get(ref.sourceIndex);
      if (!entry || !row || entry.row !== row || !sameSourceRowRef(entry.ref, ref)) return undefined;
      return entry.item;
    },
    set(ref: Ref, row: readonly unknown[] | undefined, item: Item) {
      if (row) entries.set(ref.sourceIndex, { ref, row, item });
    },
    delete(sourceIndex: number) {
      entries.delete(sourceIndex);
    },
    get size() {
      return entries.size;
    },
  };
}

export function sameSourceRowRef(a: DataGridCachedRowShape, b: DataGridCachedRowShape): boolean {
  return a.id === b.id && a.displayIndex === b.displayIndex && a.sourceIndex === b.sourceIndex && a.isDeleted === b.isDeleted && a.status === b.status;
}

/**
 * Projects a row item onto the visible columns, memoized per item object and
 * per visible-column array, so unchanged rows keep their projected object and
 * are not copied again on every recompute.
 */
export function createVisibleRowProjector<Item extends DataGridRowItemShape>() {
  let columns: readonly number[] | undefined;
  let cache = new WeakMap<Item, Item>();

  return function project(item: Item, visibleIndexes: readonly number[]): Item {
    if (visibleIndexes !== columns) {
      columns = visibleIndexes;
      cache = new WeakMap();
    }
    const cached = cache.get(item);
    if (cached) return cached;
    const projected = {
      ...item,
      data: visibleIndexes.map((index) => item.data[index]),
      isDirtyCol: visibleIndexes.map((index) => item.isDirtyCol[index] ?? false),
    } as Item;
    cache.set(item, projected);
    return projected;
  };
}
