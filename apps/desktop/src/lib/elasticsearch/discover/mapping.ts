import type { DiscoverField, DiscoverFieldType, DiscoverHit } from "./types";
import { flattenSource } from "./documents";

const NUMBER_TYPES = new Set(["long", "integer", "short", "byte", "double", "float", "half_float", "scaled_float", "unsigned_long", "token_count", "rank_feature"]);
const KEYWORD_TYPES = new Set(["keyword", "constant_keyword", "wildcard", "version"]);
const TEXT_TYPES = new Set(["text", "match_only_text", "search_as_you_type", "annotated_text"]);

export function normalizeFieldType(esType: string): DiscoverFieldType {
  if (TEXT_TYPES.has(esType)) return "text";
  if (KEYWORD_TYPES.has(esType)) return "keyword";
  if (NUMBER_TYPES.has(esType)) return "number";
  if (esType === "date" || esType === "date_nanos") return "date";
  if (esType === "boolean") return "boolean";
  if (esType === "ip") return "ip";
  if (esType === "geo_point" || esType === "geo_shape" || esType === "point" || esType === "shape") return "geo";
  if (esType === "nested") return "nested";
  if (esType === "object" || esType === "flattened" || esType === "flat_object") return "object";
  if (esType === "binary") return "binary";
  return "unknown";
}

function isAggregatable(esType: string, mapping: Record<string, unknown>): boolean {
  if (mapping.doc_values === false) return false;
  if (TEXT_TYPES.has(esType)) return mapping.fielddata === true;
  return KEYWORD_TYPES.has(esType) || NUMBER_TYPES.has(esType) || esType === "date" || esType === "date_nanos" || esType === "boolean" || esType === "ip";
}

function isSearchable(esType: string, mapping: Record<string, unknown>): boolean {
  if (mapping.index === false) return false;
  return esType !== "binary" && esType !== "object" && esType !== "nested";
}

interface RawField {
  name: string;
  esType: string;
  searchable: boolean;
  aggregatable: boolean;
  subField: boolean;
  parent?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function collectProperties(properties: Record<string, unknown>, prefix: string, out: RawField[], aliases: Array<{ name: string; path: string }>) {
  for (const [key, rawMapping] of Object.entries(properties)) {
    if (!isRecord(rawMapping)) continue;
    const name = prefix ? `${prefix}.${key}` : key;
    const esType = typeof rawMapping.type === "string" ? rawMapping.type : "object";
    if (rawMapping.enabled === false) continue;
    if (esType === "alias") {
      if (typeof rawMapping.path === "string") aliases.push({ name, path: rawMapping.path });
      continue;
    }
    if (isRecord(rawMapping.properties)) {
      // Object and nested containers are not fields themselves; their leaves are.
      collectProperties(rawMapping.properties, name, out, aliases);
      continue;
    }
    if (esType === "object") continue;
    out.push({ name, esType, searchable: isSearchable(esType, rawMapping), aggregatable: isAggregatable(esType, rawMapping), subField: false });
    if (isRecord(rawMapping.fields)) {
      for (const [subKey, subMapping] of Object.entries(rawMapping.fields)) {
        if (!isRecord(subMapping) || typeof subMapping.type !== "string") continue;
        out.push({
          name: `${name}.${subKey}`,
          esType: subMapping.type,
          searchable: isSearchable(subMapping.type, subMapping),
          aggregatable: isAggregatable(subMapping.type, subMapping),
          subField: true,
          parent: name,
        });
      }
    }
  }
}

/** Find `properties` in a `_mapping` entry (typeless 7.x+ or typed 6.x layout). */
function mappingProperties(mappings: unknown): Record<string, unknown> | null {
  if (!isRecord(mappings)) return null;
  if (isRecord(mappings.properties)) return mappings.properties;
  for (const value of Object.values(mappings)) {
    if (isRecord(value) && isRecord(value.properties)) return value.properties;
  }
  return null;
}

/** Flatten one index's mapping into leaf fields (aliases resolved to their target type). */
export function flattenIndexMapping(mappings: unknown): RawField[] {
  const properties = mappingProperties(mappings);
  if (!properties) return [];
  const fields: RawField[] = [];
  const aliases: Array<{ name: string; path: string }> = [];
  collectProperties(properties, "", fields, aliases);
  const byName = new Map(fields.map((field) => [field.name, field]));
  for (const alias of aliases) {
    const target = byName.get(alias.path);
    if (target) fields.push({ ...target, name: alias.name, subField: false, parent: undefined });
  }
  return fields;
}

export const META_FIELDS: readonly DiscoverField[] = [
  { name: "_id", type: "keyword", esTypes: ["_id"], searchable: true, aggregatable: false, subField: false },
  { name: "_index", type: "keyword", esTypes: ["_index"], searchable: true, aggregatable: true, subField: false },
];

/**
 * Merge the fields of every index in a `GET /<pattern>/_mapping` response.
 * Fields whose type differs across indices get the `conflict` type.
 */
export function fieldsFromMappingResponse(response: unknown): DiscoverField[] {
  if (!isRecord(response)) return [];
  const merged = new Map<string, DiscoverField>();
  for (const indexEntry of Object.values(response)) {
    if (!isRecord(indexEntry)) continue;
    for (const raw of flattenIndexMapping(indexEntry.mappings)) {
      const type = normalizeFieldType(raw.esType);
      const existing = merged.get(raw.name);
      if (!existing) {
        merged.set(raw.name, { name: raw.name, type, esTypes: [raw.esType], searchable: raw.searchable, aggregatable: raw.aggregatable, subField: raw.subField, parent: raw.parent });
        continue;
      }
      if (!existing.esTypes.includes(raw.esType)) existing.esTypes.push(raw.esType);
      if (existing.type !== type) existing.type = "conflict";
      existing.searchable = existing.searchable && raw.searchable;
      existing.aggregatable = existing.aggregatable && raw.aggregatable;
    }
  }
  const fields = [...merged.values()].sort((a, b) => a.name.localeCompare(b.name));
  return [...fields, ...META_FIELDS.filter((meta) => !merged.has(meta.name)).map((meta) => ({ ...meta, esTypes: [...meta.esTypes] }))];
}

/** Add fields present in loaded documents but missing from the mapping (e.g. disabled objects). */
export function withUnmappedFields(fields: readonly DiscoverField[], hits: readonly DiscoverHit[]): DiscoverField[] {
  const known = new Set(fields.map((field) => field.name));
  const extra = new Set<string>();
  for (const hit of hits) {
    for (const name of Object.keys(flattenSource(hit._source ?? {}))) {
      if (!known.has(name)) extra.add(name);
    }
  }
  if (extra.size === 0) return [...fields];
  const unmapped: DiscoverField[] = [...extra].sort().map((name) => ({ name, type: "unknown", esTypes: [], searchable: false, aggregatable: false, subField: false, unmapped: true }));
  return [...fields, ...unmapped];
}

export function dateFieldNames(fields: readonly DiscoverField[]): string[] {
  return fields.filter((field) => field.type === "date" && !field.unmapped).map((field) => field.name);
}

/** Default time field: `@timestamp` when mapped, else the first date field, else none (""). */
export function defaultTimeField(fields: readonly DiscoverField[]): string {
  const dates = dateFieldNames(fields);
  if (dates.includes("@timestamp")) return "@timestamp";
  return dates[0] ?? "";
}

export function isSortableField(field: DiscoverField | undefined): boolean {
  if (!field || field.unmapped) return false;
  return field.aggregatable && field.name !== "_id";
}
