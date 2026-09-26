/**
 * Index patterns saved in OpenSearch Dashboards / Kibana. They are saved
 * objects in the `.kibana` index (tenant copies `.kibana_*` with the security
 * plugin): `index-pattern` documents plus a `config` document whose
 * `defaultIndex` names the default pattern.
 */

export interface DashboardsIndexPattern {
  title: string;
  timeField?: string;
}

export interface DashboardsPatternsResult {
  patterns: DashboardsIndexPattern[];
  /** Title of the pattern Dashboards opens by default, when set. */
  defaultTitle?: string;
}

export const DASHBOARDS_PATTERNS_PATH = "/.kibana*/_search?ignore_unavailable=true&allow_no_indices=true";

export function dashboardsPatternsRequestBody(): string {
  return JSON.stringify({
    size: 500,
    query: { terms: { type: ["index-pattern", "config"] } },
    _source: ["type", "index-pattern.title", "index-pattern.timeFieldName", "config.defaultIndex"],
  });
}

interface SavedObjectHit {
  _id?: unknown;
  _source?: {
    type?: unknown;
    "index-pattern"?: { title?: unknown; timeFieldName?: unknown };
    config?: { defaultIndex?: unknown };
  };
}

/** Parse the saved-objects search response; tolerant of missing or odd fields. */
export function parseDashboardsPatterns(body: string): DashboardsPatternsResult {
  let hits: SavedObjectHit[] = [];
  try {
    const parsed = JSON.parse(body) as { hits?: { hits?: SavedObjectHit[] } };
    hits = Array.isArray(parsed.hits?.hits) ? parsed.hits.hits : [];
  } catch {
    return { patterns: [] };
  }
  const byTitle = new Map<string, DashboardsIndexPattern>();
  const titleById = new Map<string, string>();
  let defaultId: string | undefined;
  for (const hit of hits) {
    const source = hit._source;
    if (!source) continue;
    if (source.type === "index-pattern") {
      const title = source["index-pattern"]?.title;
      if (typeof title !== "string" || !title.trim()) continue;
      const timeField = source["index-pattern"]?.timeFieldName;
      const existing = byTitle.get(title);
      // Tenant copies of the same pattern: keep one, preferring one with a time field.
      if (!existing || (!existing.timeField && typeof timeField === "string" && timeField)) {
        byTitle.set(title, { title, ...(typeof timeField === "string" && timeField ? { timeField } : {}) });
      }
      if (typeof hit._id === "string") titleById.set(hit._id.replace(/^index-pattern:/, ""), title);
    } else if (source.type === "config" && !defaultId) {
      const value = source.config?.defaultIndex;
      if (typeof value === "string" && value) defaultId = value;
    }
  }
  const patterns = [...byTitle.values()].sort((a, b) => a.title.localeCompare(b.title));
  const defaultTitle = defaultId ? titleById.get(defaultId) : undefined;
  return defaultTitle ? { patterns, defaultTitle } : { patterns };
}
