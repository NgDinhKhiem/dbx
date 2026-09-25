import { describe, expect, it } from "vitest";
import { computeTopValues, fieldSegments, flattenSource, formatFieldValue, getHitFieldValue, HIGHLIGHT_POST_TAG, HIGHLIGHT_PRE_TAG, hitJson, parseHighlight, sourceSummary, truncateSegments } from "../documents";
import { extractErrorInfo } from "../errors";
import { indexPatternSuggestions, matchesIndexPattern, wildcardBase } from "../indexPatterns";
import { dateFieldNames, defaultTimeField, fieldsFromMappingResponse, isSortableField, withUnmappedFields } from "../mapping";
import { buildPplQuery, buildPplRequest, buildSqlRequest, parseTabularResponse } from "../sqlPpl";
import { autoInterval, parseDateMath, QUICK_RANGES, quickRangeFor, resolveTimeRange } from "../timeRange";
import type { DiscoverHit } from "../types";

const hl = (text: string) => `${HIGHLIGHT_PRE_TAG}${text}${HIGHLIGHT_POST_TAG}`;

describe("timeRange", () => {
  const now = new Date("2026-09-25T12:34:56.789Z");

  it("parses relative date math", () => {
    expect(parseDateMath("now", now)?.toISOString()).toBe("2026-09-25T12:34:56.789Z");
    expect(parseDateMath("now-15m", now)?.toISOString()).toBe("2026-09-25T12:19:56.789Z");
    expect(parseDateMath("now-24h", now)?.toISOString()).toBe("2026-09-24T12:34:56.789Z");
    expect(parseDateMath("now-7d", now)?.toISOString()).toBe("2026-09-18T12:34:56.789Z");
    expect(parseDateMath("now+1h-30m", now)?.toISOString()).toBe("2026-09-25T13:04:56.789Z");
    expect(parseDateMath("now-1y", now)?.getFullYear()).toBe(2025);
  });

  it("rounds with /unit (down for from, up for to)", () => {
    const start = parseDateMath("now/h", now);
    expect(start?.getMinutes()).toBe(0);
    expect(start?.getSeconds()).toBe(0);
    const end = parseDateMath("now/h", now, true);
    expect(end?.getMinutes()).toBe(59);
    expect(end?.getMilliseconds()).toBe(999);
  });

  it("parses absolute dates and rejects garbage", () => {
    expect(parseDateMath("2026-09-23T10:00:00Z")?.toISOString()).toBe("2026-09-23T10:00:00.000Z");
    expect(parseDateMath("1758499200000")?.getTime()).toBe(1758499200000);
    expect(parseDateMath("now-abc", now)).toBeNull();
    expect(parseDateMath("not a date")).toBeNull();
    expect(parseDateMath("")).toBeNull();
  });

  it("resolves ranges and rejects inverted ones", () => {
    expect(resolveTimeRange({ from: "now-1h", to: "now" }, now)?.from.toISOString()).toBe("2026-09-25T11:34:56.789Z");
    expect(resolveTimeRange({ from: "now", to: "now-1h" }, now)).toBeNull();
  });

  it("finds quick ranges", () => {
    expect(QUICK_RANGES.map((range) => range.from)).toEqual(["now-15m", "now-1h", "now-4h", "now-24h", "now-7d", "now-30d", "now-90d", "now-1y"]);
    expect(quickRangeFor({ from: "now-7d", to: "now" })?.key).toBe("last7d");
    expect(quickRangeFor({ from: "now-8d", to: "now" })).toBeUndefined();
  });

  it("picks nice intervals near 50 buckets", () => {
    const minute = 60_000;
    const hour = 60 * minute;
    const day = 24 * hour;
    expect(autoInterval(15 * minute).expression).toBe("15s");
    expect(autoInterval(hour).expression).toBe("1m");
    expect(autoInterval(4 * hour).expression).toBe("5m");
    expect(autoInterval(day).expression).toBe("30m");
    expect(autoInterval(7 * day).expression).toBe("3h");
    expect(autoInterval(30 * day).expression).toBe("12h");
    expect(autoInterval(90 * day).expression).toBe("2d");
    expect(autoInterval(365 * day).expression).toBe("7d");
    for (const span of [minute, 15 * minute, hour, day, 7 * day, 30 * day, 365 * day, 10 * 365 * day]) {
      expect(span / autoInterval(span).ms).toBeLessThanOrEqual(100);
    }
  });
});

describe("mapping", () => {
  const response = {
    "logs-local-2026.09.24": {
      mappings: {
        properties: {
          "@timestamp": { type: "date" },
          timestamp: { type: "date" },
          level: { type: "keyword" },
          severity: { type: "integer" },
          message: { type: "text", fields: { keyword: { type: "keyword", ignore_above: 256 } } },
          service: { properties: { name: { type: "keyword" } } },
          kubernetes: { type: "object" },
          disabled: { type: "object", enabled: false },
          Log_Level: { type: "alias", path: "level" },
          payload: { type: "binary" },
        },
      },
    },
    "logs-local-2026.09.25": {
      mappings: { properties: { severity: { type: "keyword" }, host: { properties: { ip: { type: "ip" } } } } },
    },
  };

  it("flattens and merges mappings across indices", () => {
    const fields = fieldsFromMappingResponse(response);
    const byName = Object.fromEntries(fields.map((field) => [field.name, field]));
    expect(Object.keys(byName)).toEqual(["@timestamp", "host.ip", "level", "Log_Level", "message", "message.keyword", "payload", "service.name", "severity", "timestamp", "_id", "_index"]);
    expect(byName["message"]).toMatchObject({ type: "text", aggregatable: false, searchable: true });
    expect(byName["message.keyword"]).toMatchObject({ type: "keyword", subField: true, parent: "message", aggregatable: true });
    expect(byName["Log_Level"]).toMatchObject({ type: "keyword", esTypes: ["keyword"] });
    expect(byName["severity"]).toMatchObject({ type: "conflict", esTypes: ["integer", "keyword"] });
    expect(byName["payload"]).toMatchObject({ type: "binary", searchable: false });
    expect(byName["service.name"].type).toBe("keyword");
  });

  it("supports typed (6.x) mappings", () => {
    const fields = fieldsFromMappingResponse({ old: { mappings: { doc: { properties: { ts: { type: "date" } } } } } });
    expect(fields[0]).toMatchObject({ name: "ts", type: "date" });
  });

  it("chooses the default time field", () => {
    const fields = fieldsFromMappingResponse(response);
    expect(dateFieldNames(fields)).toEqual(["@timestamp", "timestamp"]);
    expect(defaultTimeField(fields)).toBe("@timestamp");
    expect(defaultTimeField(fields.filter((field) => field.name !== "@timestamp"))).toBe("timestamp");
    expect(defaultTimeField([])).toBe("");
  });

  it("adds fields only present in documents", () => {
    const fields = fieldsFromMappingResponse(response);
    const hits: DiscoverHit[] = [{ _index: "i", _id: "1", _source: { level: "INFO", kubernetes: { pod: { name: "web-1" } } } }];
    const merged = withUnmappedFields(fields, hits);
    expect(merged.find((field) => field.name === "kubernetes.pod.name")).toMatchObject({ type: "unknown", unmapped: true });
  });

  it("knows which fields are sortable", () => {
    const fields = fieldsFromMappingResponse(response);
    const find = (name: string) => fields.find((field) => field.name === name);
    expect(isSortableField(find("@timestamp"))).toBe(true);
    expect(isSortableField(find("message"))).toBe(false);
    expect(isSortableField(find("level"))).toBe(true);
    expect(isSortableField(find("_id"))).toBe(false);
  });
});

describe("documents", () => {
  const hit: DiscoverHit = {
    _index: "logs-local-2026.09.23",
    _id: "abc",
    _source: { level: "ERROR", message: "boom <script>", service: { name: "api" }, tags: ["a", "b"], nested: [{ x: 1 }], empty: {} },
    highlight: { message: [`${hl("boom")} <script>`] },
  };

  it("flattens _source into dotted paths", () => {
    expect(flattenSource(hit._source ?? {})).toEqual({ level: "ERROR", message: "boom <script>", "service.name": "api", tags: ["a", "b"], nested: [{ x: 1 }], empty: {} });
  });

  it("reads meta fields, dotted paths and multi-field parents", () => {
    expect(getHitFieldValue(hit, "_id")).toBe("abc");
    expect(getHitFieldValue(hit, "_index")).toBe("logs-local-2026.09.23");
    expect(getHitFieldValue(hit, "service.name")).toBe("api");
    expect(getHitFieldValue(hit, "message.keyword", { parent: "message" })).toBe("boom <script>");
    expect(getHitFieldValue(hit, "missing")).toBeUndefined();
  });

  it("formats values", () => {
    expect(formatFieldValue(["a", 1, null])).toBe("a, 1, null");
    expect(formatFieldValue({ a: 1 })).toBe('{"a":1}');
    expect(formatFieldValue(null)).toBe("null");
    expect(formatFieldValue(false)).toBe("false");
  });

  it("parses highlight markers into segments without interpreting HTML", () => {
    expect(parseHighlight(`a ${hl("b")} c ${hl("<d>")}`)).toEqual([
      { text: "a ", highlighted: false },
      { text: "b", highlighted: true },
      { text: " c ", highlighted: false },
      { text: "<d>", highlighted: true },
    ]);
    expect(parseHighlight(`${HIGHLIGHT_PRE_TAG}open`)).toEqual([{ text: "open", highlighted: true }]);
    expect(parseHighlight("")).toEqual([]);
  });

  it("truncates segments", () => {
    expect(truncateSegments([{ text: "abcdef", highlighted: false }], 3)).toEqual([{ text: "abc…", highlighted: false }]);
  });

  it("builds field segments and a source summary with highlighted fields first", () => {
    expect(fieldSegments(hit, "message")).toEqual([
      { text: "boom", highlighted: true },
      { text: " <script>", highlighted: false },
    ]);
    expect(fieldSegments(hit, "level")).toEqual([{ text: "ERROR", highlighted: false }]);
    const summary = sourceSummary(hit);
    expect(summary.map((entry) => entry.field)).toEqual(["message", "level", "service.name", "tags", "nested", "empty"]);
  });

  it("computes top values from loaded documents", () => {
    const hits: DiscoverHit[] = [
      { _index: "i", _id: "1", _source: { level: "INFO", tags: ["a", "b"] } },
      { _index: "i", _id: "2", _source: { level: "INFO", tags: ["a"] } },
      { _index: "i", _id: "3", _source: { level: "ERROR" } },
      { _index: "i", _id: "4", _source: {} },
    ];
    const level = computeTopValues(hits, "level");
    expect(level.exists).toBe(3);
    expect(level.total).toBe(4);
    expect(level.values).toEqual([
      { value: "INFO", label: "INFO", count: 2, percent: 50 },
      { value: "ERROR", label: "ERROR", count: 1, percent: 25 },
    ]);
    expect(computeTopValues(hits, "tags").values[0]).toMatchObject({ value: "a", count: 2 });
    expect(computeTopValues(hits, "level", 1).values).toHaveLength(1);
  });

  it("renders the JSON view with _id and _index", () => {
    expect(JSON.parse(hitJson(hit))).toMatchObject({ _index: "logs-local-2026.09.23", _id: "abc", _source: { level: "ERROR" } });
  });
});

describe("indexPatterns", () => {
  it("derives wildcard bases", () => {
    expect(wildcardBase("logs-local-2026.09.20")).toBe("logs-local-*");
    expect(wildcardBase("logs-local-2026.09.*")).toBe("logs-local-*");
    expect(wildcardBase("transaction_index_vnm_local_2025_1")).toBe("transaction_index_vnm_local_*");
    expect(wildcardBase(".ds-logs-000001")).toBe(".ds-logs-*");
    expect(wildcardBase("products")).toBeNull();
    expect(wildcardBase("v2")).toBeNull();
  });

  it("matches index patterns", () => {
    expect(matchesIndexPattern("logs-local-2026.09.20", "logs-*")).toBe(true);
    expect(matchesIndexPattern("logs-local-2026.09.20", "metrics-*,logs-local-*")).toBe(true);
    expect(matchesIndexPattern("logs-local-2026.09.20", "logs-*,-logs-local-*")).toBe(false);
    expect(matchesIndexPattern("logs", "logs-*")).toBe(false);
  });

  it("suggests derived patterns, aliases and indices", () => {
    const indices = ["logs-local-2026.09.20", "logs-local-2026.09.21", "products", ".kibana_1", "tx_2025_1"];
    const all = indexPatternSuggestions({ indices, aliases: [".kibana", "current-logs"], typed: "" });
    expect(all.map((entry) => entry.value)).toEqual(["logs-local-*", "tx_*", "current-logs", "logs-local-2026.09.20", "logs-local-2026.09.21", "products", "tx_2025_1"]);
    expect(all[0]).toEqual({ value: "logs-local-*", kind: "pattern", matches: 2 });
    expect(indexPatternSuggestions({ indices, aliases: [], typed: "logs-local-*" }).map((entry) => entry.value)).toEqual(["logs-local-*", "logs-local-2026.09.20", "logs-local-2026.09.21"]);
    expect(indexPatternSuggestions({ indices, aliases: [".kibana"], typed: ".k" }).map((entry) => entry.value)).toEqual([".kibana_*", ".kibana", ".kibana_1"]);
    expect(indexPatternSuggestions({ indices, aliases: [], typed: "logs-local-2026.09." })[0]).toEqual({ value: "logs-local-*", kind: "pattern", matches: 2 });
  });
});

describe("errors", () => {
  it("prefers the failed shard cause of search errors", () => {
    const body = JSON.stringify({
      error: {
        root_cause: [{ type: "query_shard_exception", reason: "Failed to parse query [level:(WARN]" }],
        type: "search_phase_execution_exception",
        reason: "all shards failed",
        failed_shards: [{ shard: 0, reason: { type: "query_shard_exception", reason: "Failed to parse query [level:(WARN]", caused_by: { type: "parse_exception", reason: "Cannot parse 'level:(WARN'" } } }],
      },
      status: 400,
    });
    expect(extractErrorInfo(body, 400)).toEqual({ message: "query_shard_exception: Failed to parse query [level:(WARN]", detail: "parse_exception: Cannot parse 'level:(WARN'", status: 400 });
  });

  it("falls back to root_cause and plugin errors", () => {
    expect(extractErrorInfo(JSON.stringify({ error: { root_cause: [{ type: "index_not_found_exception", reason: "no such index [x]" }], type: "index_not_found_exception", reason: "no such index [x]" }, status: 404 }), 404).message).toBe("index_not_found_exception: no such index [x]");
    const ppl = extractErrorInfo(JSON.stringify({ error: { reason: "Error occurred in OpenSearch engine: no such index [nope]", details: "IndexNotFoundException", type: "IndexNotFoundException" }, status: 404 }), 404);
    expect(ppl).toEqual({ message: "IndexNotFoundException: Error occurred in OpenSearch engine: no such index [nope]", detail: "IndexNotFoundException", status: 404 });
    expect(extractErrorInfo("Unauthorized", 401).message).toBe("Unauthorized");
    expect(extractErrorInfo("", 502).message).toBe("HTTP 502");
    expect(extractErrorInfo(JSON.stringify({ error: "plain" })).message).toBe("plain");
  });
});

describe("sqlPpl", () => {
  it("prefixes the PPL source when missing", () => {
    expect(buildPplQuery("", "logs-*")).toBe("source=logs-*");
    expect(buildPplQuery("where level='ERROR' | head 5", "logs-*")).toBe("source=logs-* | where level='ERROR' | head 5");
    expect(buildPplQuery("| stats count() by level", "logs-*")).toBe("source=logs-* | stats count() by level");
    expect(buildPplQuery("source=other | head 1", "logs-*")).toBe("source=other | head 1");
    expect(buildPplQuery("search source = other", "logs-*")).toBe("search source = other");
    expect(buildPplRequest("head 1", "logs-*")).toEqual({ method: "POST", path: "/_plugins/_ppl", body: JSON.stringify({ query: "source=logs-* | head 1" }) });
  });

  it("builds SQL requests per distribution", () => {
    expect(buildSqlRequest("select 1", "logs-*", "opensearch")).toEqual({ method: "POST", path: "/_plugins/_sql?format=jdbc", body: JSON.stringify({ query: "select 1" }) });
    expect(buildSqlRequest("select 1", "logs-*", "elasticsearch")).toEqual({ method: "POST", path: "/_sql?format=json", body: JSON.stringify({ query: "select 1" }) });
    expect(JSON.parse(buildSqlRequest("", "logs-*", "opensearch").body ?? "{}").query).toBe("SELECT * FROM `logs-*` LIMIT 100");
  });

  it("parses JDBC and Elasticsearch SQL responses", () => {
    expect(
      parseTabularResponse(
        JSON.stringify({
          schema: [
            { name: "level", type: "keyword" },
            { name: "count(*)", alias: "c", type: "integer" },
          ],
          datarows: [["INFO", 3]],
          total: 1,
          size: 1,
        }),
      ),
    ).toEqual({
      columns: [
        { name: "level", type: "keyword" },
        { name: "c", type: "integer" },
      ],
      rows: [["INFO", 3]],
      total: 1,
    });
    expect(parseTabularResponse(JSON.stringify({ columns: [{ name: "a", type: "long" }], rows: [[1], [2]] }))).toEqual({ columns: [{ name: "a", type: "long" }], rows: [[1], [2]], total: 2 });
    expect(() => parseTabularResponse("{}")).toThrow();
  });
});
