import { describe, expect, it } from "vitest";
import { DqlSyntaxError } from "../dql";
import { HIGHLIGHT_POST_TAG, HIGHLIGHT_PRE_TAG } from "../documents";
import { buildQueryClause, buildSearchBody, describeFilter, effectiveSort, filterToDsl, readHistogramBuckets, readTotalHits, toggleFilterNegation } from "../requestBuilder";
import type { DiscoverFilter } from "../types";

const range = { from: new Date("2026-09-24T00:00:00.000Z"), to: new Date("2026-09-25T00:00:00.000Z") };

function pill(partial: Partial<DiscoverFilter>): DiscoverFilter {
  return { id: "x", field: "level", operator: "is", value: "ERROR", enabled: true, ...partial };
}

describe("buildQueryClause", () => {
  it("uses query_string with analyze_wildcard for Lucene", () => {
    expect(buildQueryClause("lucene", "level:ERROR AND msg*")).toEqual({ query_string: { query: "level:ERROR AND msg*", analyze_wildcard: true } });
  });

  it("parses DQL and throws syntax errors", () => {
    expect(buildQueryClause("dql", "level:ERROR")).toEqual({ match: { level: "ERROR" } });
    expect(buildQueryClause("dql", "  ")).toEqual({ match_all: {} });
    expect(() => buildQueryClause("dql", "level:(")).toThrow(DqlSyntaxError);
  });
});

describe("filterToDsl", () => {
  it("translates each operator like Dashboards", () => {
    expect(filterToDsl(pill({}))).toEqual({ clause: { match_phrase: { level: "ERROR" } }, negate: false });
    expect(filterToDsl(pill({ operator: "is_not" }))).toEqual({ clause: { match_phrase: { level: "ERROR" } }, negate: true });
    expect(filterToDsl(pill({ operator: "exists", value: undefined }))).toEqual({ clause: { exists: { field: "level" } }, negate: false });
    expect(filterToDsl(pill({ operator: "not_exists" }))).toEqual({ clause: { exists: { field: "level" } }, negate: true });
    expect(filterToDsl(pill({ field: "severity", operator: "between", range: { gte: "30", lt: "50" } }))).toEqual({ clause: { range: { severity: { gte: "30", lt: "50" } } }, negate: false });
    expect(filterToDsl(pill({ field: "severity", operator: "between", range: { gte: "30" } }))).toEqual({ clause: { range: { severity: { gte: "30" } } }, negate: false });
    expect(filterToDsl(pill({ operator: "between", range: {} }))).toBeNull();
    expect(filterToDsl(pill({ field: "" }))).toBeNull();
    expect(filterToDsl(pill({ value: false }))).toEqual({ clause: { match_phrase: { level: false } }, negate: false });
  });

  it("describes and toggles pills", () => {
    expect(describeFilter(pill({ operator: "is_not" }))).toEqual({ field: "level", value: "ERROR", negated: true });
    expect(describeFilter(pill({ operator: "exists" }))).toEqual({ field: "level", value: "exists", negated: false });
    expect(describeFilter(pill({ operator: "between", range: { gte: "1" } }))).toEqual({ field: "level", value: "1 → +∞", negated: false });
    expect(toggleFilterNegation(pill({})).operator).toBe("is_not");
    expect(toggleFilterNegation(pill({ operator: "not_exists" })).operator).toBe("exists");
  });
});

describe("buildSearchBody", () => {
  it("builds the full Discover request", () => {
    const { body, interval } = buildSearchBody({
      language: "dql",
      query: "level:ERROR",
      filters: [pill({ field: "service.name", value: "api" }), pill({ field: "is_error", operator: "is_not", value: true }), pill({ field: "x", enabled: false })],
      fields: [],
      timeField: "@timestamp",
      range,
      sort: [],
      histogram: true,
      timeZone: "UTC",
    });
    expect(interval).toEqual({ expression: "30m", ms: 1_800_000 });
    expect(body).toEqual({
      size: 100,
      track_total_hits: true,
      query: {
        bool: {
          must: [{ match: { level: "ERROR" } }],
          filter: [{ range: { "@timestamp": { gte: "2026-09-24T00:00:00.000Z", lte: "2026-09-25T00:00:00.000Z", format: "strict_date_optional_time" } } }, { match_phrase: { "service.name": "api" } }],
          must_not: [{ match_phrase: { is_error: true } }],
        },
      },
      highlight: { pre_tags: [HIGHLIGHT_PRE_TAG], post_tags: [HIGHLIGHT_POST_TAG], fields: { "*": {} }, fragment_size: 2147483647 },
      sort: [{ "@timestamp": { order: "desc", unmapped_type: "boolean" } }],
      aggs: {
        histogram: {
          date_histogram: {
            field: "@timestamp",
            fixed_interval: "30m",
            min_doc_count: 0,
            extended_bounds: { min: range.from.getTime(), max: range.to.getTime() },
            time_zone: "UTC",
          },
        },
      },
    });
  });

  it("omits the time filter, sort and histogram without a time field", () => {
    const { body, interval } = buildSearchBody({ language: "lucene", query: "", filters: [], fields: [], timeField: "", range, sort: [], histogram: true });
    expect(interval).toBeUndefined();
    expect(body.query).toEqual({ match_all: {} });
    expect(body.sort).toBeUndefined();
    expect(body.aggs).toBeUndefined();
  });

  it("supports explicit sort and paging for load more", () => {
    const { body } = buildSearchBody({ language: "dql", query: "", filters: [], fields: [], timeField: "@timestamp", range, sort: [{ field: "severity", direction: "asc" }], size: 100, from: 100 });
    expect(body.from).toBe(100);
    expect(body.sort).toEqual([{ severity: { order: "asc", unmapped_type: "boolean" } }]);
    expect(body.aggs).toBeUndefined();
    expect(effectiveSort([], null)).toEqual([]);
  });
});

describe("response readers", () => {
  it("reads totals and histogram buckets", () => {
    expect(readTotalHits({ hits: { total: { value: 10000, relation: "gte" } } })).toEqual({ value: 10000, relation: "gte" });
    expect(readTotalHits({ hits: { total: 5 } })).toEqual({ value: 5, relation: "eq" });
    expect(readTotalHits({})).toEqual({ value: 0, relation: "eq" });
    expect(readHistogramBuckets({ aggregations: { histogram: { buckets: [{ key: 1, doc_count: 2, key_as_string: "x" }, { key: "bad" }] } } })).toEqual([{ key: 1, count: 2 }]);
    expect(readHistogramBuckets({})).toEqual([]);
  });
});
