import type { QueryTab } from "@/types/database";

/** A Discover tab keeps its view state as JSON in `tab.sql`. */
export function parseDiscoverTabState(sql: string | undefined): Record<string, unknown> | undefined {
  if (!sql?.trim()) return undefined;
  try {
    const value: unknown = JSON.parse(sql);
    return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

/** Index pattern a Discover tab was opened for ("" for a connection-level tab). */
export function discoverTabIndexPattern(tab: Pick<QueryTab, "sql">): string {
  const pattern = parseDiscoverTabState(tab.sql)?.indexPattern;
  return typeof pattern === "string" ? pattern : "";
}
