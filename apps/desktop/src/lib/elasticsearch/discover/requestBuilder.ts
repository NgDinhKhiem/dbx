import { dqlToDsl, type EsQuery } from "./dql";
import { HIGHLIGHT_POST_TAG, HIGHLIGHT_PRE_TAG } from "./documents";
import { autoInterval, type ResolvedTimeRange } from "./timeRange";
import type { DiscoverField, DiscoverFilter, DiscoverQueryLanguage, DiscoverSort } from "./types";

export const INITIAL_PAGE_SIZE = 100;
export const MAX_SAMPLE_SIZE = 500;

/** Main query clause for DQL / Lucene. Throws `DqlSyntaxError` for bad DQL. */
export function buildQueryClause(language: DiscoverQueryLanguage, query: string, fields: readonly DiscoverField[] = []): EsQuery {
  const text = query.trim();
  if (!text) return { match_all: {} };
  if (language === "lucene") return { query_string: { query: text, analyze_wildcard: true } };
  return dqlToDsl(text, { fields });
}

/** DSL for a filter pill (positive form) plus whether it belongs in `must_not`. */
export function filterToDsl(filter: DiscoverFilter): { clause: EsQuery; negate: boolean } | null {
  if (!filter.field) return null;
  switch (filter.operator) {
    case "is":
    case "is_not":
      if (filter.value === undefined) return null;
      return { clause: { match_phrase: { [filter.field]: filter.value } }, negate: filter.operator === "is_not" };
    case "exists":
    case "not_exists":
      return { clause: { exists: { field: filter.field } }, negate: filter.operator === "not_exists" };
    case "between": {
      const bounds: Record<string, string> = {};
      if (filter.range?.gte !== undefined && filter.range.gte !== "") bounds.gte = filter.range.gte;
      if (filter.range?.lt !== undefined && filter.range.lt !== "") bounds.lt = filter.range.lt;
      if (Object.keys(bounds).length === 0) return null;
      return { clause: { range: { [filter.field]: bounds } }, negate: false };
    }
  }
}

export function timeRangeClause(timeField: string, range: ResolvedTimeRange): EsQuery {
  return { range: { [timeField]: { gte: range.from.toISOString(), lte: range.to.toISOString(), format: "strict_date_optional_time" } } };
}

export interface SearchBodyOptions {
  language: DiscoverQueryLanguage;
  query: string;
  filters: readonly DiscoverFilter[];
  fields: readonly DiscoverField[];
  /** Empty string or null when the pattern has no time field. */
  timeField: string | null;
  range: ResolvedTimeRange | null;
  sort: readonly DiscoverSort[];
  size?: number;
  from?: number;
  /** Include the histogram aggregation (first page only). */
  histogram?: boolean;
  timeZone?: string;
}

export interface SearchBody {
  body: Record<string, unknown>;
  /** `fixed_interval` used by the histogram, when requested. */
  interval?: { expression: string; ms: number };
}

/** Effective sort: explicit sort, else the time field descending. */
export function effectiveSort(sort: readonly DiscoverSort[], timeField: string | null): DiscoverSort[] {
  if (sort.length > 0) return [...sort];
  return timeField ? [{ field: timeField, direction: "desc" }] : [];
}

/** Build the `_search` body for DQL / Lucene mode, like Dashboards Discover. */
export function buildSearchBody(options: SearchBodyOptions): SearchBody {
  const queryClause = buildQueryClause(options.language, options.query, options.fields);
  const filter: EsQuery[] = [];
  const mustNot: EsQuery[] = [];
  const timeField = options.timeField || null;
  if (timeField && options.range) filter.push(timeRangeClause(timeField, options.range));
  for (const pill of options.filters) {
    if (!pill.enabled) continue;
    const dsl = filterToDsl(pill);
    if (!dsl) continue;
    (dsl.negate ? mustNot : filter).push(dsl.clause);
  }
  const bool: Record<string, unknown> = {};
  if (!("match_all" in queryClause)) bool.must = [queryClause];
  if (filter.length > 0) bool.filter = filter;
  if (mustNot.length > 0) bool.must_not = mustNot;

  const body: Record<string, unknown> = {
    size: options.size ?? INITIAL_PAGE_SIZE,
    track_total_hits: true,
    query: Object.keys(bool).length > 0 ? { bool } : { match_all: {} },
    highlight: {
      pre_tags: [HIGHLIGHT_PRE_TAG],
      post_tags: [HIGHLIGHT_POST_TAG],
      fields: { "*": {} },
      fragment_size: 2147483647,
    },
  };
  if (options.from && options.from > 0) body.from = options.from;
  const sort = effectiveSort(options.sort, timeField);
  if (sort.length > 0) body.sort = sort.map((entry) => ({ [entry.field]: { order: entry.direction, unmapped_type: "boolean" } }));

  let interval: SearchBody["interval"];
  if (options.histogram && timeField && options.range) {
    interval = autoInterval(options.range.to.getTime() - options.range.from.getTime());
    const dateHistogram: Record<string, unknown> = {
      field: timeField,
      fixed_interval: interval.expression,
      min_doc_count: 0,
      extended_bounds: { min: options.range.from.getTime(), max: options.range.to.getTime() },
    };
    if (options.timeZone) dateHistogram.time_zone = options.timeZone;
    body.aggs = { histogram: { date_histogram: dateHistogram } };
  }
  return { body, interval };
}

export interface HistogramBucket {
  key: number;
  count: number;
}

export function readHistogramBuckets(response: unknown): HistogramBucket[] {
  const buckets = (response as { aggregations?: { histogram?: { buckets?: Array<{ key?: unknown; doc_count?: unknown }> } } })?.aggregations?.histogram?.buckets;
  if (!Array.isArray(buckets)) return [];
  return buckets.filter((bucket) => typeof bucket.key === "number").map((bucket) => ({ key: bucket.key as number, count: typeof bucket.doc_count === "number" ? bucket.doc_count : 0 }));
}

export function readTotalHits(response: unknown): { value: number; relation: "eq" | "gte" } {
  const total = (response as { hits?: { total?: unknown } })?.hits?.total;
  if (typeof total === "number") return { value: total, relation: "eq" };
  if (total && typeof total === "object") {
    const value = (total as { value?: unknown }).value;
    const relation = (total as { relation?: unknown }).relation;
    return { value: typeof value === "number" ? value : 0, relation: relation === "gte" ? "gte" : "eq" };
  }
  return { value: 0, relation: "eq" };
}

let filterSequence = 0;

export function createFilterId(): string {
  filterSequence += 1;
  return `f${Date.now().toString(36)}${filterSequence.toString(36)}`;
}

/** Short label for a pill, e.g. `level: ERROR`, `NOT level: ERROR`, `trace_id: exists`. */
export function describeFilter(filter: DiscoverFilter): { field: string; value: string; negated: boolean } {
  switch (filter.operator) {
    case "is":
    case "is_not":
      return { field: filter.field, value: filter.value === null ? "null" : String(filter.value ?? ""), negated: filter.operator === "is_not" };
    case "exists":
    case "not_exists":
      return { field: filter.field, value: "exists", negated: filter.operator === "not_exists" };
    case "between":
      return { field: filter.field, value: `${filter.range?.gte || "-∞"} → ${filter.range?.lt || "+∞"}`, negated: false };
  }
}

/** Flip a pill between include and exclude. */
export function toggleFilterNegation(filter: DiscoverFilter): DiscoverFilter {
  const flipped: Partial<Record<DiscoverFilter["operator"], DiscoverFilter["operator"]>> = { is: "is_not", is_not: "is", exists: "not_exists", not_exists: "exists" };
  const operator = flipped[filter.operator];
  return operator ? { ...filter, operator } : { ...filter };
}
