import type { DiscoverField, DiscoverHit, HighlightSegment } from "./types";

/** Highlight markers sent as `pre_tags` / `post_tags`; parsed back into segments, never rendered as HTML. */
export const HIGHLIGHT_PRE_TAG = "@dbx-discover-highlight@";
export const HIGHLIGHT_POST_TAG = "@/dbx-discover-highlight@";

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

/**
 * Flatten `_source` into dotted paths like Dashboards' `flattenHit`: nested
 * objects become `a.b.c`, arrays are kept as values.
 */
export function flattenSource(source: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  const walk = (value: Record<string, unknown>, prefix: string) => {
    for (const [key, child] of Object.entries(value)) {
      const name = prefix ? `${prefix}.${key}` : key;
      if (isPlainObject(child) && Object.keys(child).length > 0) {
        walk(child, name);
      } else {
        out[name] = child;
      }
    }
  };
  walk(source, "");
  return out;
}

const flattenCache = new WeakMap<object, Record<string, unknown>>();

/** Cached `flattenSource` for a hit (hits are immutable once loaded). */
export function flattenHit(hit: DiscoverHit): Record<string, unknown> {
  let flat = flattenCache.get(hit);
  if (!flat) {
    flat = flattenSource(hit._source ?? {});
    flattenCache.set(hit, flat);
  }
  return flat;
}

/** Value of a field in a hit (`_id`/`_index` meta fields, dotted paths, multi-field parent fallback). */
export function getHitFieldValue(hit: DiscoverHit, fieldName: string, field?: Pick<DiscoverField, "parent">): unknown {
  if (fieldName === "_id") return hit._id;
  if (fieldName === "_index") return hit._index;
  if (fieldName === "_score") return hit._score ?? null;
  const flat = flattenHit(hit);
  if (fieldName in flat) return flat[fieldName];
  if (field?.parent && field.parent in flat) return flat[field.parent];
  const docValue = hit.fields?.[fieldName];
  if (docValue !== undefined) return docValue.length === 1 ? docValue[0] : docValue;
  return undefined;
}

/** Display text for a value (strings raw, everything else JSON). */
export function formatFieldValue(value: unknown): string {
  if (value === undefined) return "";
  if (value === null) return "null";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean" || typeof value === "bigint") return String(value);
  if (Array.isArray(value) && value.every((item) => item === null || ["string", "number", "boolean"].includes(typeof item))) {
    return value.map((item) => (item === null ? "null" : String(item))).join(", ");
  }
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

/** Split highlighter output into plain / highlighted segments using the marker tags. */
export function parseHighlight(text: string, pre = HIGHLIGHT_PRE_TAG, post = HIGHLIGHT_POST_TAG): HighlightSegment[] {
  const segments: HighlightSegment[] = [];
  let rest = text;
  while (rest.length > 0) {
    const start = rest.indexOf(pre);
    if (start < 0) {
      segments.push({ text: rest, highlighted: false });
      break;
    }
    if (start > 0) segments.push({ text: rest.slice(0, start), highlighted: false });
    const afterPre = rest.slice(start + pre.length);
    const end = afterPre.indexOf(post);
    if (end < 0) {
      segments.push({ text: afterPre, highlighted: true });
      break;
    }
    segments.push({ text: afterPre.slice(0, end), highlighted: true });
    rest = afterPre.slice(end + post.length);
  }
  // Merge adjacent segments of the same kind.
  return segments.reduce<HighlightSegment[]>((merged, segment) => {
    const last = merged[merged.length - 1];
    if (last && last.highlighted === segment.highlighted) last.text += segment.text;
    else if (segment.text) merged.push({ ...segment });
    return merged;
  }, []);
}

/** Truncate long plain text, keeping highlighted segments intact where possible. */
export function truncateSegments(segments: HighlightSegment[], maxLength: number): HighlightSegment[] {
  const out: HighlightSegment[] = [];
  let used = 0;
  for (const segment of segments) {
    if (used >= maxLength) {
      out.push({ text: "…", highlighted: false });
      break;
    }
    const remaining = maxLength - used;
    if (segment.text.length > remaining) {
      out.push({ text: `${segment.text.slice(0, remaining)}…`, highlighted: segment.highlighted });
      used = maxLength;
      break;
    }
    out.push(segment);
    used += segment.text.length;
  }
  return out;
}

/** Segments for a field in a hit: highlighted fragments when present, else the formatted value. */
export function fieldSegments(hit: DiscoverHit, fieldName: string, maxLength = 2000, field?: Pick<DiscoverField, "parent">): HighlightSegment[] {
  const highlight = hit.highlight?.[fieldName];
  if (highlight && highlight.length > 0) {
    const segments: HighlightSegment[] = [];
    highlight.forEach((fragment, index) => {
      if (index > 0) segments.push({ text: ", ", highlighted: false });
      segments.push(...parseHighlight(fragment));
    });
    return truncateSegments(segments, maxLength);
  }
  const text = formatFieldValue(getHitFieldValue(hit, fieldName, field));
  return truncateSegments([{ text, highlighted: false }], maxLength);
}

export interface SourceSummaryEntry {
  field: string;
  segments: HighlightSegment[];
}

/**
 * The `_source` column: `field: value` pairs, highlighted fields first
 * (like Dashboards), each value truncated.
 */
export function sourceSummary(hit: DiscoverHit, maxFields = 40, maxValueLength = 300): SourceSummaryEntry[] {
  const flat = flattenHit(hit);
  const names = Object.keys(flat);
  const highlighted = names.filter((name) => hit.highlight?.[name]?.length);
  const rest = names.filter((name) => !hit.highlight?.[name]?.length);
  return [...highlighted, ...rest].slice(0, maxFields).map((field) => ({ field, segments: fieldSegments(hit, field, maxValueLength) }));
}

export interface TopValue {
  value: unknown;
  label: string;
  count: number;
  percent: number;
}

export interface FieldValueStats {
  values: TopValue[];
  /** Documents containing the field. */
  exists: number;
  /** Documents sampled. */
  total: number;
}

/**
 * Top values of a field over the loaded documents, like Dashboards' field
 * details: array values count each element, percentages are relative to
 * the number of sampled documents.
 */
export function computeTopValues(hits: readonly DiscoverHit[], fieldName: string, limit = 5, field?: Pick<DiscoverField, "parent">): FieldValueStats {
  const counts = new Map<string, { value: unknown; count: number }>();
  let exists = 0;
  for (const hit of hits) {
    const value = getHitFieldValue(hit, fieldName, field);
    if (value === undefined || value === null) continue;
    exists += 1;
    const values = Array.isArray(value) ? value : [value];
    for (const item of values) {
      const key = typeof item === "string" ? `s:${item}` : `j:${JSON.stringify(item)}`;
      const entry = counts.get(key);
      if (entry) entry.count += 1;
      else counts.set(key, { value: item, count: 1 });
    }
  }
  const total = hits.length;
  const values = [...counts.values()]
    .sort((a, b) => b.count - a.count)
    .slice(0, limit)
    .map((entry) => ({ value: entry.value, label: formatFieldValue(entry.value), count: entry.count, percent: total > 0 ? (entry.count / total) * 100 : 0 }));
  return { values, exists, total };
}

/** Pretty JSON for the expanded document view. */
export function hitJson(hit: DiscoverHit): string {
  return JSON.stringify({ _index: hit._index, _id: hit._id, _score: hit._score ?? null, _source: hit._source ?? {} }, null, 2);
}
