export type IndexSuggestionKind = "dashboards" | "pattern" | "index" | "alias";

export interface IndexSuggestion {
  value: string;
  kind: IndexSuggestionKind;
  /** Number of indices a derived wildcard pattern matches. */
  matches?: number;
  /** Time field of a pattern saved in Dashboards. */
  timeField?: string;
}

/**
 * Wildcard base of an index name: trailing date/number parts are replaced by
 * `*` (`logs-local-2026.09.20` -> `logs-local-*`, `tx_2025_1` -> `tx_*`).
 * Returns null when the name has no such suffix.
 */
export function wildcardBase(name: string): string | null {
  const trimmed = name.replace(/\*+$/, "");
  const match = /^(.*?[^\d._-][._-])\d[\d._-]*$/.exec(trimmed);
  return match ? `${match[1]}*` : null;
}

function wildcardRegExp(pattern: string): RegExp {
  return new RegExp(`^${pattern.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*")}$`);
}

/** Does an index name match a (comma separated, `-exclusion`-aware) index pattern? */
export function matchesIndexPattern(name: string, pattern: string): boolean {
  let matched = false;
  for (const part of pattern
    .split(",")
    .map((item) => item.trim())
    .filter(Boolean)) {
    if (part.startsWith("-")) {
      if (wildcardRegExp(part.slice(1)).test(name)) matched = false;
    } else if (wildcardRegExp(part).test(name)) {
      matched = true;
    }
  }
  return matched;
}

export interface SuggestionInput {
  indices: readonly string[];
  aliases: readonly string[];
  /** Index patterns saved in OpenSearch Dashboards / Kibana, listed first. */
  dashboardsPatterns?: readonly { title: string; timeField?: string }[];
  typed: string;
  limit?: number;
}

/**
 * Suggestions for the index pattern combobox: derived wildcard patterns
 * first (most indices first), then aliases, then concrete indices. Hidden
 * (dot-prefixed) names only show when the typed text starts with a dot.
 */
export function indexPatternSuggestions({ indices, aliases, dashboardsPatterns = [], typed, limit = 50 }: SuggestionInput): IndexSuggestion[] {
  const needle = typed.trim().replace(/\*/g, "").toLowerCase();
  const showHidden = needle.startsWith(".");
  const visible = (name: string) => showHidden || !name.startsWith(".");
  const matchesNeedle = (name: string) => !needle || name.toLowerCase().includes(needle);

  const patternCounts = new Map<string, number>();
  for (const index of indices) {
    if (!visible(index)) continue;
    const base = wildcardBase(index);
    if (base) patternCounts.set(base, (patternCounts.get(base) ?? 0) + 1);
  }
  const typedBase = wildcardBase(typed.trim());
  if (typedBase && !patternCounts.has(typedBase)) {
    const count = indices.filter((index) => matchesIndexPattern(index, typedBase)).length;
    if (count > 0) patternCounts.set(typedBase, count);
  }

  const patterns: IndexSuggestion[] = [...patternCounts.entries()]
    .filter(([pattern]) => pattern === typedBase || matchesNeedle(pattern))
    .sort((a, b) => Number(b[0] === typedBase) - Number(a[0] === typedBase) || b[1] - a[1] || a[0].localeCompare(b[0]))
    .map(([value, matches]) => ({ value, kind: "pattern", matches }));
  const aliasSuggestions: IndexSuggestion[] = [...new Set(aliases)]
    .filter((alias) => visible(alias) && matchesNeedle(alias))
    .sort()
    .map((value) => ({ value, kind: "alias" }));
  const indexSuggestions: IndexSuggestion[] = [...new Set(indices)]
    .filter((index) => visible(index) && matchesNeedle(index))
    .sort()
    .map((value) => ({ value, kind: "index" }));
  const saved: IndexSuggestion[] = dashboardsPatterns.filter((pattern) => matchesNeedle(pattern.title)).map((pattern) => ({ value: pattern.title, kind: "dashboards" as const, ...(pattern.timeField ? { timeField: pattern.timeField } : {}) }));
  const savedValues = new Set(saved.map((item) => item.value));
  const derived = patterns.filter((item) => !savedValues.has(item.value));
  return [...saved, ...derived, ...aliasSuggestions, ...indexSuggestions].slice(0, limit);
}
