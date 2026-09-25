/** Width rules for resizable Discover document-table columns. */

export const DISCOVER_MIN_COLUMN_WIDTH = 60;
export const DISCOVER_MAX_COLUMN_WIDTH = 2000;
/** Width the time column starts with before it is resized. */
export const DISCOVER_DEFAULT_TIME_COLUMN_WIDTH = 192;
/** Room kept for each column without an explicit width, so resized columns never squeeze it to nothing. */
export const DISCOVER_AUTO_COLUMN_MIN_WIDTH = 160;
/** Width key of the time column (kept apart from a data column with the same field name). */
export const DISCOVER_TIME_COLUMN_KEY = "\u0000time";
const MAX_SAVED_WIDTHS = 200;

export function clampColumnWidth(width: number): number {
  if (!Number.isFinite(width)) return DISCOVER_MIN_COLUMN_WIDTH;
  return Math.round(Math.min(DISCOVER_MAX_COLUMN_WIDTH, Math.max(DISCOVER_MIN_COLUMN_WIDTH, width)));
}

/** Clean widths restored from a saved tab: finite numbers only, clamped, bounded in count. */
export function sanitizeColumnWidths(value: unknown): Record<string, number> {
  const widths: Record<string, number> = {};
  if (!value || typeof value !== "object" || Array.isArray(value)) return widths;
  let count = 0;
  for (const [key, width] of Object.entries(value as Record<string, unknown>)) {
    if (count >= MAX_SAVED_WIDTHS) break;
    if (!key || typeof width !== "number" || !Number.isFinite(width)) continue;
    widths[key] = clampColumnWidth(width);
    count++;
  }
  return widths;
}

/** Set (or with `null`, clear) one column's width, returning a new map. */
export function withColumnWidth(widths: Readonly<Record<string, number>>, key: string, width: number | null): Record<string, number> {
  const next = { ...widths };
  if (width === null) delete next[key];
  else next[key] = clampColumnWidth(width);
  return next;
}

/**
 * Minimum table width: explicit widths plus room for the auto-sized columns.
 * With a fixed table layout, auto columns would otherwise collapse once the
 * explicit widths exceed the viewport.
 */
export function discoverTableMinWidth(leadingWidth: number, widths: readonly (number | undefined)[]): number {
  return widths.reduce<number>((sum, width) => sum + (width ?? DISCOVER_AUTO_COLUMN_MIN_WIDTH), leadingWidth);
}
