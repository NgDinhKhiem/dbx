import type { ClusterDistribution, TabularResult } from "./types";
import type { DiscoverRawRequest } from "./discoverApi";

const SOURCE_PREFIX = /^\s*(search\s+)?source\s*=/i;

/** Prefix `source=<pattern> | ` when a PPL query does not name its source. */
export function buildPplQuery(query: string, indexPattern: string): string {
  const trimmed = query.trim().replace(/^\|\s*/, "");
  if (SOURCE_PREFIX.test(trimmed)) return trimmed;
  const source = `source=${indexPattern.trim()}`;
  return trimmed ? `${source} | ${trimmed}` : source;
}

export function buildPplRequest(query: string, indexPattern: string): DiscoverRawRequest {
  return { method: "POST", path: "/_plugins/_ppl", body: JSON.stringify({ query: buildPplQuery(query, indexPattern) }) };
}

/** Default SQL when the editor is empty. Index patterns with dashes/wildcards need backquotes. */
export function defaultSqlQuery(indexPattern: string): string {
  return `SELECT * FROM \`${indexPattern.trim()}\` LIMIT 100`;
}

export function buildSqlRequest(query: string, indexPattern: string, distribution: ClusterDistribution): DiscoverRawRequest {
  const sql = query.trim() || defaultSqlQuery(indexPattern);
  if (distribution === "elasticsearch") return { method: "POST", path: "/_sql?format=json", body: JSON.stringify({ query: sql }) };
  return { method: "POST", path: "/_plugins/_sql?format=jdbc", body: JSON.stringify({ query: sql }) };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function readColumns(value: unknown): Array<{ name: string; type?: string }> | null {
  if (!Array.isArray(value)) return null;
  return value.map((column, index) => {
    if (isRecord(column)) {
      const name = typeof column.alias === "string" && column.alias ? column.alias : typeof column.name === "string" ? column.name : `column_${index + 1}`;
      return { name, type: typeof column.type === "string" ? column.type : undefined };
    }
    return { name: String(column) };
  });
}

/**
 * Normalize SQL/PPL responses: OpenSearch JDBC (`schema` + `datarows`) and
 * Elasticsearch SQL JSON (`columns` + `rows`).
 */
export function parseTabularResponse(body: string): TabularResult {
  const parsed: unknown = JSON.parse(body);
  if (!isRecord(parsed)) throw new Error("Unexpected response");
  const jdbcColumns = readColumns(parsed.schema);
  if (jdbcColumns) {
    const rows = Array.isArray(parsed.datarows) ? (parsed.datarows as unknown[][]) : [];
    return { columns: jdbcColumns, rows, total: typeof parsed.total === "number" ? parsed.total : rows.length };
  }
  const esColumns = readColumns(parsed.columns);
  if (esColumns) {
    const rows = Array.isArray(parsed.rows) ? (parsed.rows as unknown[][]) : [];
    return { columns: esColumns, rows, total: rows.length };
  }
  throw new Error("Unexpected response: no schema/columns");
}
