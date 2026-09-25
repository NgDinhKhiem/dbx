/** Query languages the Discover view supports. */
export type DiscoverQueryLanguage = "dql" | "lucene" | "ppl" | "sql";

export type DiscoverSortDirection = "asc" | "desc";

export interface DiscoverSort {
  field: string;
  direction: DiscoverSortDirection;
}

/** A time range expressed with date math (`now-15m`) or absolute ISO timestamps. */
export interface DiscoverTimeRange {
  from: string;
  to: string;
}

export type DiscoverFilterOperator = "is" | "is_not" | "exists" | "not_exists" | "between";

/** A filter pill. `is_not` / `not_exists` are the negated forms of `is` / `exists`. */
export interface DiscoverFilter {
  id: string;
  field: string;
  operator: DiscoverFilterOperator;
  /** Value for `is` / `is_not`. */
  value?: string | number | boolean | null;
  /** Bounds for `between` (inclusive `gte`, exclusive `lt` like Dashboards). */
  range?: { gte?: string; lt?: string };
  enabled: boolean;
}

/** Serializable Discover view state (persisted in the tab). */
export interface DiscoverState {
  indexPattern: string;
  /** Empty string means "No time field". `undefined` means "pick a default from the mapping". */
  timeField?: string;
  timeRange: DiscoverTimeRange;
  language: DiscoverQueryLanguage;
  query: string;
  filters: DiscoverFilter[];
  columns: string[];
  sort: DiscoverSort[];
}

export type DiscoverFieldType = "string" | "text" | "keyword" | "number" | "date" | "boolean" | "ip" | "geo" | "object" | "nested" | "binary" | "conflict" | "unknown";

export interface DiscoverField {
  /** Dotted path, e.g. `kubernetes.pod.name` or `message.keyword`. */
  name: string;
  /** Normalized type used for the icon and query decisions. */
  type: DiscoverFieldType;
  /** Raw Elasticsearch mapping type(s), e.g. `keyword`, `long`, `date`. */
  esTypes: string[];
  searchable: boolean;
  aggregatable: boolean;
  /** Multi-field sub-field such as `message.keyword`. */
  subField: boolean;
  /** Parent field of a multi-field (`message` for `message.keyword`). */
  parent?: string;
  /** Found in documents but not in the mapping. */
  unmapped?: boolean;
}

export interface DiscoverHit {
  _index: string;
  _id: string;
  _score?: number | null;
  _source?: Record<string, unknown>;
  fields?: Record<string, unknown[]>;
  highlight?: Record<string, string[]>;
  sort?: unknown[];
}

export interface HighlightSegment {
  text: string;
  highlighted: boolean;
}

export interface TabularResult {
  columns: Array<{ name: string; type?: string }>;
  rows: unknown[][];
  total?: number;
}

export type ClusterDistribution = "opensearch" | "elasticsearch";
