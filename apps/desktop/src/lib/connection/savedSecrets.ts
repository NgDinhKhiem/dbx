import type { ConnectionConfig, PluginFormField, TransportLayerConfig } from "@/types/database";

/**
 * Stored connection secrets never reach the frontend. `loadConnections()`
 * returns each connection with its secret fields blanked and lists the stored
 * ones in `saved_secrets`; the backend merges stored secrets back by
 * connection id whenever a config is sent to it (save, test, connect...).
 *
 * Path names shared with the backend:
 * - `password`, `redis_sentinel_password`, `connection_string`, `init_script`,
 *   `url_params` (at least one sensitive URL param value is hidden)
 * - `transport_layers.<seg>.password|key_passphrase|token` where `<seg>` is the
 *   layer id, or `#<index>` when the id is empty
 * - `external_config.<path>` for MQ / MQTT / Nacos / Cassandra secret fields
 * - `connection_secrets.<key>` for plugin secrets
 */

export type TransportLayerSecretField = "password" | "key_passphrase" | "token";

type SecretMetadata = Pick<ConnectionConfig, "saved_secrets" | "cleared_secrets">;

export const PASSWORD_SECRET_PATH = "password";
export const REDIS_SENTINEL_PASSWORD_SECRET_PATH = "redis_sentinel_password";
export const CONNECTION_STRING_SECRET_PATH = "connection_string";
export const INIT_SCRIPT_SECRET_PATH = "init_script";
export const URL_PARAMS_SECRET_PATH = "url_params";

const TOP_LEVEL_SECRET_FIELDS = ["password", "redis_sentinel_password", "connection_string", "init_script"] as const;

/** Secret fields inside `external_config`, per connection type (mirrors the backend). */
const EXTERNAL_CONFIG_SECRET_FIELDS: Record<string, readonly string[]> = {
  mq: ["auth.token", "auth.password", "auth.value", "auth.clientSecret", "tokenSigning.key"],
  mqtt: ["auth.password"],
  nacos: ["auth.password", "rnacosConsoleAuth.password"],
  cassandra: ["tls.truststore_password", "tls.keystore_password"],
};

function uniq(values: Iterable<string>): string[] {
  return Array.from(new Set(values));
}

export function hasSavedSecret(config: SecretMetadata | null | undefined, path: string): boolean {
  if (!config?.saved_secrets?.includes(path)) return false;
  return !config.cleared_secrets?.includes(path);
}

export function isSecretCleared(config: SecretMetadata | null | undefined, path: string): boolean {
  return !!config?.cleared_secrets?.includes(path);
}

/** `true` when the value is typed in the form, or a saved value will be used for the blank field. */
export function secretAvailable(config: SecretMetadata | null | undefined, path: string, value: unknown): boolean {
  if (typeof value === "string" ? value.length > 0 : value != null && value !== false) return true;
  return hasSavedSecret(config, path);
}

export function transportLayerSecretSegment(layer: Pick<TransportLayerConfig, "id"> | null | undefined, index: number): string {
  return layer?.id ? layer.id : `#${index}`;
}

export function transportLayerSecretPath(layer: Pick<TransportLayerConfig, "id"> | null | undefined, index: number, field: TransportLayerSecretField): string {
  return `transport_layers.${transportLayerSecretSegment(layer, index)}.${field}`;
}

export function externalConfigSecretPath(path: string): string {
  return `external_config.${path}`;
}

export function connectionSecretPath(key: string): string {
  return `connection_secrets.${key}`;
}

/** Mark a stored secret for deletion on the next save. Mutates and returns `config`. */
export function clearSavedSecret<T extends SecretMetadata>(config: T, path: string): T {
  config.saved_secrets = (config.saved_secrets ?? []).filter((item) => item !== path);
  if (!config.cleared_secrets?.includes(path)) config.cleared_secrets = [...(config.cleared_secrets ?? []), path];
  return config;
}

export function isSensitiveUrlParamKey(key: string): boolean {
  const normalized = key.toLowerCase().replace(/[^a-z0-9]/g, "");
  return /password|passphrase|secret|token|apikey|privatekey|clientsecret/.test(normalized);
}

function mapUrlParams(value: string, mapPart: (key: string, rawValue: string, part: string) => string): string {
  return value
    .split(/([&;])/)
    .map((part, index) => {
      if (index % 2 === 1) return part;
      const separator = part.indexOf("=");
      if (separator < 0) return part;
      return mapPart(part.slice(0, separator), part.slice(separator + 1), part);
    })
    .join("");
}

/** Whether a URL-param string carries a non-empty value for a sensitive key. */
export function urlParamsHaveSensitiveValue(value: string | null | undefined): boolean {
  if (typeof value !== "string" || !value) return false;
  let found = false;
  mapUrlParams(value, (key, rawValue, part) => {
    if (isSensitiveUrlParamKey(key) && rawValue.trim()) found = true;
    return part;
  });
  return found;
}

/** Blank only the values of sensitive URL params (`a=1&password=` style), keeping everything else. */
export function redactUrlParams<T extends string | null | undefined>(value: T): T {
  if (typeof value !== "string" || !value) return value;
  return mapUrlParams(value, (key, _rawValue, part) => (isSensitiveUrlParamKey(key) ? `${key}=` : part)) as T;
}

/**
 * Drop sensitive URL params whose value is blank (hidden stored secrets), so
 * copied URLs carry no `password=` artifacts. Other params are kept as-is.
 */
export function withoutBlankSensitiveUrlParams<T extends string | null | undefined>(value: T): T {
  if (typeof value !== "string" || !value) return value;
  const kept: string[] = [];
  let separator = "";
  for (const [index, part] of value.split(/([&;])/).entries()) {
    if (index % 2 === 1) {
      separator = part;
      continue;
    }
    const equals = part.indexOf("=");
    if (equals >= 0 && isSensitiveUrlParamKey(part.slice(0, equals)) && !part.slice(equals + 1).trim()) continue;
    kept.push(kept.length ? `${separator}${part}` : part);
  }
  return kept.join("") as T;
}

function externalSecretFields(config: Pick<ConnectionConfig, "db_type">): readonly string[] {
  return EXTERNAL_CONFIG_SECRET_FIELDS[config.db_type] ?? [];
}

function readNested(value: unknown, path: string[]): unknown {
  let current = value;
  for (const segment of path) {
    if (!current || typeof current !== "object" || Array.isArray(current)) return undefined;
    current = (current as Record<string, unknown>)[segment];
  }
  return current;
}

function isApiKeyAuth(externalConfig: unknown): boolean {
  const kind = readNested(externalConfig, ["auth", "kind"]);
  return typeof kind === "string" && kind.toLowerCase().replace(/[^a-z]/g, "") === "apikey";
}

function externalSecretApplies(config: Pick<ConnectionConfig, "db_type" | "external_config">, field: string): boolean {
  return field !== "auth.value" || isApiKeyAuth(config.external_config);
}

function isNonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

/** Every secret path whose value is currently typed (non-empty) in `config`. */
export function presentSecretPaths(config: ConnectionConfig): string[] {
  const paths: string[] = [];
  for (const field of TOP_LEVEL_SECRET_FIELDS) {
    if (field === "password" && config.save_password === false) continue;
    if (isNonEmptyString(config[field])) paths.push(field);
  }
  if (urlParamsHaveSensitiveValue(config.url_params)) paths.push(URL_PARAMS_SECRET_PATH);
  (config.transport_layers ?? []).forEach((layer, index) => {
    const values: [TransportLayerSecretField, unknown][] =
      layer.type === "ssh"
        ? [
            ["password", layer.password],
            ["key_passphrase", layer.key_passphrase],
          ]
        : layer.type === "proxy"
          ? [["password", layer.password]]
          : [["token", layer.token]];
    for (const [field, value] of values) {
      if (isNonEmptyString(value)) paths.push(transportLayerSecretPath(layer, index, field));
    }
  });
  for (const field of externalSecretFields(config)) {
    if (!externalSecretApplies(config, field)) continue;
    if (isNonEmptyString(readNested(config.external_config, field.split(".")))) paths.push(externalConfigSecretPath(field));
  }
  for (const [key, value] of Object.entries(config.connection_secrets ?? {})) {
    if (isNonEmptyString(value)) paths.push(connectionSecretPath(key));
  }
  return paths;
}

function blankExternalConfigSecrets(config: ConnectionConfig): unknown {
  const fields = externalSecretFields(config).filter((field) => externalSecretApplies(config, field));
  if (!fields.length || !config.external_config || typeof config.external_config !== "object") return config.external_config;
  const copy = JSON.parse(JSON.stringify(config.external_config)) as Record<string, unknown>;
  for (const field of fields) {
    const segments = field.split(".");
    const parent = readNested(copy, segments.slice(0, -1));
    if (!parent || typeof parent !== "object" || Array.isArray(parent)) continue;
    const key = segments[segments.length - 1]!;
    if (isNonEmptyString((parent as Record<string, unknown>)[key])) (parent as Record<string, unknown>)[key] = "";
  }
  return copy;
}

function transportLayerSegments(config: ConnectionConfig): Set<string> {
  return new Set((config.transport_layers ?? []).map((layer, index) => transportLayerSecretSegment(layer, index)));
}

/**
 * Local view of a connection after it was persisted: every typed secret is
 * blanked (the backend now holds it) and `saved_secrets` is recomputed as the
 * union of the previously saved paths and the paths that were typed, minus the
 * cleared ones. One-shot request fields are dropped.
 */
export function redactSavedConnection(config: ConnectionConfig): ConnectionConfig {
  const cleared = new Set(config.cleared_secrets ?? []);
  const segments = transportLayerSegments(config);
  const saved = uniq([...(config.saved_secrets ?? []), ...presentSecretPaths(config)]).filter((path) => {
    if (cleared.has(path)) return false;
    if (path === PASSWORD_SECRET_PATH && config.save_password === false) return false;
    if (path.startsWith("transport_layers.")) {
      const segment = path.slice("transport_layers.".length, path.lastIndexOf("."));
      return segments.has(segment);
    }
    return true;
  });

  const { cleared_secrets: _cleared, secrets_from_connection_id: _source, ...rest } = config;
  const redacted: ConnectionConfig = { ...rest, password: "", saved_secrets: saved };
  if (config.redis_sentinel_password) redacted.redis_sentinel_password = "";
  if (config.connection_string) redacted.connection_string = undefined;
  if (config.init_script) redacted.init_script = undefined;
  if (config.url_params) redacted.url_params = redactUrlParams(config.url_params);
  if (config.connection_secrets && Object.keys(config.connection_secrets).length) redacted.connection_secrets = {};
  if (config.transport_layers) {
    redacted.transport_layers = config.transport_layers.map((layer) => {
      const copy = { ...layer } as TransportLayerConfig;
      if (copy.type === "ssh") {
        if (copy.password) copy.password = "";
        if (copy.key_passphrase) copy.key_passphrase = "";
      } else if (copy.type === "proxy") {
        if (copy.password) copy.password = "";
      } else if (copy.token) {
        copy.token = "";
      }
      return copy;
    });
  }
  if (config.external_config !== undefined) redacted.external_config = blankExternalConfigSecrets(config);
  return redacted;
}

/** Secret path of a plugin form field, or `null` for non-secret fields. */
export function pluginFieldSecretPath(field: Pick<PluginFormField, "key" | "binding" | "type">): string | null {
  const binding = field.binding || (field.type === "password" ? "secret" : "config");
  if (binding === "password") return PASSWORD_SECRET_PATH;
  if (binding === "secret") return connectionSecretPath(field.key);
  return null;
}

/** Connection types whose submitted config never uses the top-level field. */
const CONNECTION_STRING_UNUSED_DB_TYPES = new Set(["mq", "mqtt", "nacos", "consul", "influxdb", "influxdb3", "victoriametrics", "prestosql", "gaussdb"]);
const URL_PARAMS_UNUSED_DB_TYPES = new Set(["mq", "mqtt", "nacos", "consul", "manticoresearch"]);
const PASSWORD_UNUSED_DB_TYPES = new Set(["mq", "mqtt"]);

function externalString(config: Pick<ConnectionConfig, "external_config">, path: string): string {
  const value = readNested(config.external_config, path.split("."));
  return typeof value === "string" ? value : "";
}

/**
 * Whether a stored secret is still used by the config the form is about to
 * submit. A blank secret field normally keeps its stored value; secrets that
 * the submitted config no longer uses (auth mode switched, layer removed,
 * connection type changed...) must be cleared explicitly instead of lingering
 * and being merged back by the backend.
 */
export function savedSecretApplies(config: ConnectionConfig, path: string): boolean {
  if (path === PASSWORD_SECRET_PATH) {
    if (config.save_password === false || PASSWORD_UNUSED_DB_TYPES.has(config.db_type)) return false;
    if (config.db_type === "nacos") return readNested(config.external_config, ["auth", "kind"]) === "usernamePassword";
    return true;
  }
  if (path === REDIS_SENTINEL_PASSWORD_SECRET_PATH) return config.db_type === "redis" && config.redis_connection_mode === "sentinel";
  if (path === CONNECTION_STRING_SECRET_PATH) return !CONNECTION_STRING_UNUSED_DB_TYPES.has(config.db_type);
  if (path === URL_PARAMS_SECRET_PATH) {
    if (URL_PARAMS_UNUSED_DB_TYPES.has(config.db_type)) return false;
    // The user removed every sensitive key: nothing is left to merge into.
    let hasSensitiveKey = false;
    mapUrlParams(config.url_params ?? "", (key, _value, part) => {
      if (isSensitiveUrlParamKey(key)) hasSensitiveKey = true;
      return part;
    });
    return hasSensitiveKey;
  }
  if (path === INIT_SCRIPT_SECRET_PATH) return true;
  if (path.startsWith("transport_layers.")) {
    const field = path.slice(path.lastIndexOf(".") + 1);
    const segment = path.slice("transport_layers.".length, path.lastIndexOf("."));
    const index = (config.transport_layers ?? []).findIndex((layer, layerIndex) => transportLayerSecretSegment(layer, layerIndex) === segment);
    const layer = config.transport_layers?.[index];
    if (!layer || layer.profile_id) return false;
    if (layer.type === "ssh") {
      const method = layer.auth_method;
      if (field === "password") return !method || method === "password" || method === "key+password";
      if (field === "key_passphrase") return (!method || method === "key" || method === "key+password") && !!layer.key_path?.trim();
      return false;
    }
    if (layer.type === "proxy") return field === "password";
    return field === "token";
  }
  if (path.startsWith("external_config.")) {
    const field = path.slice("external_config.".length);
    if (!externalSecretFields(config).includes(field)) return false;
    const authKind = readNested(config.external_config, ["auth", "kind"]);
    switch (field) {
      case "auth.token":
        return authKind === "token";
      case "auth.password":
        return config.db_type === "mq" ? authKind === "basic" : config.db_type === "mqtt" ? authKind === "password" : authKind === "usernamePassword";
      case "auth.value":
        return isApiKeyAuth(config.external_config);
      case "auth.clientSecret":
        return authKind === "oauth2";
      case "tokenSigning.key":
        return !!readNested(config.external_config, ["tokenSigning"]);
      case "rnacosConsoleAuth.password":
        return readNested(config.external_config, ["rnacosConsoleAuth", "kind"]) === "usernamePassword";
      case "tls.truststore_password":
        return !!externalString(config, "tls.truststore_path").trim();
      case "tls.keystore_password":
        return !!externalString(config, "tls.keystore_path").trim();
      default:
        return true;
    }
  }
  if (path.startsWith("connection_secrets.")) return config.db_type === "plugin";
  return true;
}

/** Drop local secret bookkeeping (used for export bundles and imported configs). */
export function stripSecretMetadata(config: ConnectionConfig): ConnectionConfig {
  const { saved_secrets: _saved, cleared_secrets: _cleared, secrets_from_connection_id: _source, ...rest } = config;
  return rest;
}
