import type { ConnectionConfig, ConnectionTestResult, DatabaseConnectionInfo, IdentifierCase } from "@/types/database";

export type DatabaseInfoField = keyof DatabaseConnectionInfo;

export interface DatabaseInfoRow {
  key: DatabaseInfoField;
  value: string;
}

// These connections have no real database metadata. VictoriaMetrics exposes a
// configured label as a synthetic database node, not a server database.
const DATABASE_INFO_UNSUPPORTED_TYPES = new Set<ConnectionConfig["db_type"]>(["dynamodb", "elasticsearch", "easysearch", "meilisearch", "solr", "qdrant", "weaviate", "chromadb", "etcd", "zookeeper", "nacos", "consul", "mq", "mqtt", "victoriametrics"]);

export function supportsConnectionDatabaseInfo(dbType: ConnectionConfig["db_type"]): boolean {
  return !DATABASE_INFO_UNSUPPORTED_TYPES.has(dbType);
}

const DATABASE_INFO_FIELDS: readonly DatabaseInfoField[] = ["productName", "productVersion", "currentDatabase", "serverComment", "serverCharset", "serverCollation", "unquotedIdentifierCase", "quotedIdentifierCase", "driverName", "driverVersion", "jdbcVersion"];
const IDENTIFIER_CASES = new Set<IdentifierCase>(["lower", "upper", "mixed"]);

function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value);
}

function nonBlankString(value: unknown): string | undefined {
  if (typeof value !== "string") return undefined;
  const trimmed = value.trim();
  return trimmed || undefined;
}

function identifierCase(value: unknown): IdentifierCase | undefined {
  const normalized = nonBlankString(value)?.toLowerCase() as IdentifierCase | undefined;
  return normalized && IDENTIFIER_CASES.has(normalized) ? normalized : undefined;
}

export function configuredDatabaseProductName(config: Pick<ConnectionConfig, "db_type" | "driver_label">): string {
  return config.driver_label?.trim() || config.db_type;
}

export function normalizeDatabaseConnectionInfo(value: unknown, fallbackProductName?: string, fallbackCurrentDatabase?: string): DatabaseConnectionInfo | undefined {
  const source = isRecord(value) ? value : {};
  const result: DatabaseConnectionInfo = {
    productName: nonBlankString(source.productName) ?? nonBlankString(fallbackProductName),
    productVersion: nonBlankString(source.productVersion),
    currentDatabase: nonBlankString(source.currentDatabase) ?? nonBlankString(fallbackCurrentDatabase),
    serverComment: nonBlankString(source.serverComment),
    serverCharset: nonBlankString(source.serverCharset),
    serverCollation: nonBlankString(source.serverCollation),
    unquotedIdentifierCase: identifierCase(source.unquotedIdentifierCase),
    quotedIdentifierCase: identifierCase(source.quotedIdentifierCase),
    driverName: nonBlankString(source.driverName),
    driverVersion: nonBlankString(source.driverVersion),
    jdbcVersion: nonBlankString(source.jdbcVersion),
  };
  return DATABASE_INFO_FIELDS.some((key) => result[key] !== undefined) ? result : undefined;
}

export function normalizeConnectionTestResult(value: unknown, config: ConnectionConfig): ConnectionTestResult {
  const fallbackProductName = configuredDatabaseProductName(config);
  const fallbackCurrentDatabase = config.database;
  if (typeof value === "string") {
    return {
      message: value,
      databaseInfo: normalizeDatabaseConnectionInfo(undefined, fallbackProductName, fallbackCurrentDatabase),
    };
  }
  if (!isRecord(value) || typeof value.message !== "string") {
    throw new Error("Invalid connection test response");
  }
  return {
    message: value.message,
    databaseInfo: normalizeDatabaseConnectionInfo(value.databaseInfo, fallbackProductName, fallbackCurrentDatabase),
  };
}

function stableValue(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(stableValue);
  if (!isRecord(value)) return value;
  const result: Record<string, unknown> = {};
  for (const key of Object.keys(value).sort()) {
    const child = value[key];
    if (child !== undefined) result[key] = stableValue(child);
  }
  return result;
}

function fnv1a(value: string, seed: number): number {
  let hash = seed >>> 0;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

// Secret values stay in the backend: the in-memory copy of a saved connection
// carries blanks plus `saved_secrets`, while a config submitted for connecting
// may still hold the typed value. Neither describes a different endpoint, so
// secrets and their bookkeeping are left out of the fingerprint.
const FINGERPRINT_SECRET_FIELDS = ["password", "redis_sentinel_password", "saved_secrets", "cleared_secrets", "secrets_from_connection_id"] as const;
const FINGERPRINT_LAYER_SECRET_FIELDS = ["password", "key_passphrase", "token"] as const;

function withoutSecrets(config: Record<string, unknown>): Record<string, unknown> {
  const copy: Record<string, unknown> = { ...config };
  for (const field of FINGERPRINT_SECRET_FIELDS) delete copy[field];
  if (Array.isArray(copy.transport_layers)) {
    copy.transport_layers = copy.transport_layers.map((layer) => {
      if (!layer || typeof layer !== "object") return layer;
      const layerCopy: Record<string, unknown> = { ...(layer as Record<string, unknown>) };
      for (const field of FINGERPRINT_LAYER_SECRET_FIELDS) delete layerCopy[field];
      return layerCopy;
    });
  }
  return copy;
}

/**
 * Whether a config submitted by the UI changes a secret compared with the
 * previous in-memory copy: a typed value that differs from what that copy
 * holds (saved copies normally hold blanks) or an explicit clear.
 */
export function hasSubmittedSecretChange(config: ConnectionConfig, previous?: ConnectionConfig): boolean {
  const next = config as unknown as Record<string, unknown>;
  const before = (previous ?? {}) as unknown as Record<string, unknown>;
  if (Array.isArray(next.cleared_secrets) && next.cleared_secrets.length > 0) return true;
  const changed = (value: unknown, old: unknown) => typeof value === "string" && value.length > 0 && value !== old;
  const valueFields = FINGERPRINT_SECRET_FIELDS.filter((field) => field === "password" || field === "redis_sentinel_password");
  if (valueFields.some((field) => changed(next[field], before[field]))) return true;
  const layers = Array.isArray(next.transport_layers) ? next.transport_layers : [];
  const previousLayers = Array.isArray(before.transport_layers) ? before.transport_layers : [];
  return layers.some((layer) => {
    if (!layer || typeof layer !== "object") return false;
    const current = layer as Record<string, unknown>;
    const old = (previousLayers.find((candidate) => !!candidate && typeof candidate === "object" && (candidate as Record<string, unknown>).id === current.id) ?? {}) as Record<string, unknown>;
    return FINGERPRINT_LAYER_SECRET_FIELDS.some((field) => changed(current[field], old[field]));
  });
}

/**
 * Identity of a connection's settings. Secrets are left out by default (see
 * above); `includeSecrets` compares them too, for two drafts that both carry
 * typed values (e.g. "was this exact draft tested?").
 */
export function connectionConfigFingerprint(config: ConnectionConfig, sourceName = config.name, options?: { includeSecrets?: boolean }): string {
  const { database_info: _databaseInfo, default_schema: _defaultSchema, note: _note, ...submittedConfig } = config;
  const comparable = options?.includeSecrets ? submittedConfig : withoutSecrets(submittedConfig as Record<string, unknown>);
  const serialized = JSON.stringify(stableValue({ config: comparable, sourceName }));
  const first = fnv1a(serialized, 0x811c9dc5).toString(16).padStart(8, "0");
  const second = fnv1a(serialized, 0x9e3779b9).toString(16).padStart(8, "0");
  return `${first}${second}`;
}

export function databaseInfoRows(info: DatabaseConnectionInfo): DatabaseInfoRow[] {
  return DATABASE_INFO_FIELDS.flatMap((key) => {
    const value = info[key];
    return value ? [{ key, value }] : [];
  });
}

export function databaseInfoSummary(info: DatabaseConnectionInfo): string {
  return [info.productName, info.productVersion].filter(Boolean).join(" ");
}

export function databaseInfoCopyText(info: DatabaseConnectionInfo, fieldLabel: (field: DatabaseInfoField) => string, caseLabel: (value: IdentifierCase) => string): string {
  return databaseInfoRows(info)
    .map((row) => {
      const value = row.key === "unquotedIdentifierCase" || row.key === "quotedIdentifierCase" ? caseLabel(row.value as IdentifierCase) : row.value;
      return `${fieldLabel(row.key)}: ${value}`;
    })
    .join("\n");
}

export function isTauriCommandUnavailable(error: unknown, command: string): boolean {
  const message = error instanceof Error ? error.message : String(error);
  const escapedCommand = command.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`unknown command\\s*[:=]?\\s*['"]?${escapedCommand}['"]?`, "i").test(message) || new RegExp(`command\\s+['"]?${escapedCommand}['"]?\\s+(?:was\\s+)?not found`, "i").test(message);
}
