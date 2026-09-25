import type { AiConfig, AiConfigItem } from "@/types/ai";

/**
 * AI config secrets (API key, credentialed proxy URL, custom header values and
 * CLI environment values) never reach the frontend: the backend returns them
 * blank and lists the stored ones in `savedSecrets` by path. A blank value on
 * save keeps the stored secret, `clearedSecrets` deletes it explicitly, and AI
 * request commands resolve the stored values by `configId`.
 */
export const AI_API_KEY_SECRET = "apiKey";
export const AI_PROXY_URL_SECRET = "proxyUrl";
export const AI_CUSTOM_HEADERS_FIELD = "customHeaders";
export const AI_CLI_ENV_FIELDS = ["codexCliEnv", "claudeCodeCliEnv", "piAgentCliEnv", "opencodeCliEnv", "cursorCliEnv", "grokCliEnv", "codebuddyCliEnv", "qoderCliEnv"] as const;
export type AiCliEnvField = (typeof AI_CLI_ENV_FIELDS)[number];

type SecretAware = Pick<AiConfig, "savedSecrets"> | null | undefined;

export function aiHeaderSecretPath(name: string): string {
  return `${AI_CUSTOM_HEADERS_FIELD}.${name.trim()}`;
}

export function aiEnvSecretPath(field: AiCliEnvField, key: string): string {
  return `${field}.${key.trim()}`;
}

export function hasSavedAiSecret(config: SecretAware, path: string): boolean {
  return Array.isArray(config?.savedSecrets) && config.savedSecrets.includes(path);
}

/** Whether a usable API key exists, either typed in this session or stored by the backend. */
export function hasAiApiKey(config: (Pick<AiConfig, "apiKey" | "savedSecrets"> & Partial<Pick<AiConfig, "clearedSecrets">>) | null | undefined): boolean {
  if (!config) return false;
  if (config.apiKey?.trim()) return true;
  if (config.clearedSecrets?.includes(AI_API_KEY_SECRET)) return false;
  return hasSavedAiSecret(config, AI_API_KEY_SECRET);
}

/**
 * Stored config id the backend uses to fill redacted secrets for AI requests.
 * An explicit id wins; otherwise the id carried by an `AiConfigItem`-shaped config is used.
 */
export function aiConfigIdForRequest(config: unknown, explicit?: string | null): string | undefined {
  if (typeof explicit === "string" && explicit.trim()) return explicit;
  const id = config && typeof config === "object" ? (config as Partial<AiConfigItem>).id : undefined;
  return typeof id === "string" && id.trim() ? id : undefined;
}

export function aiProxyUrlHasCredentials(url: string | null | undefined): boolean {
  const value = url?.trim() ?? "";
  if (!value) return false;
  const authority = value.replace(/^[a-z][a-z0-9+.-]*:\/\//i, "").split(/[/?#]/)[0] ?? "";
  return authority.includes("@");
}

function secretValueFor(config: AiConfig, path: string): string | undefined {
  if (path === AI_API_KEY_SECRET) return config.apiKey ?? "";
  if (path === AI_PROXY_URL_SECRET) return config.proxyUrl ?? "";
  const dot = path.indexOf(".");
  if (dot <= 0) return undefined;
  const field = path.slice(0, dot);
  const name = path.slice(dot + 1);
  if (field === AI_CUSTOM_HEADERS_FIELD) return config.customHeaders?.[name];
  if ((AI_CLI_ENV_FIELDS as readonly string[]).includes(field)) return (config[field as AiCliEnvField] as Record<string, string> | undefined)?.[name];
  return undefined;
}

/**
 * Cleared paths that still mean something for this save: only stored secrets,
 * and never a path the user has since typed a replacement value for.
 */
export function effectiveAiClearedSecrets(config: AiConfig, cleared: Iterable<string>, saved: readonly string[] = config.savedSecrets ?? []): string[] {
  const savedSet = new Set(saved);
  const result: string[] = [];
  for (const path of cleared) {
    if (!savedSet.has(path) || result.includes(path)) continue;
    if (secretValueFor(config, path)?.trim()) continue;
    result.push(path);
  }
  return result;
}

/**
 * Mirrors the backend redaction after a successful save so the store never
 * retains typed plaintext secrets: values are blanked and `savedSecrets`
 * reflects what the backend now stores for the item.
 */
export function redactAiConfigSecrets<T extends AiConfig>(config: T, previousSaved: readonly string[] = config.savedSecrets ?? []): T {
  const cleared = new Set(config.clearedSecrets ?? []);
  const wasSaved = (path: string) => previousSaved.includes(path) && !cleared.has(path);
  const saved: string[] = [];

  const apiKey = config.apiKey?.trim() ?? "";
  if (apiKey || wasSaved(AI_API_KEY_SECRET)) saved.push(AI_API_KEY_SECRET);

  let proxyUrl = config.proxyUrl ?? "";
  if (proxyUrl.trim()) {
    if (aiProxyUrlHasCredentials(proxyUrl)) {
      saved.push(AI_PROXY_URL_SECRET);
      proxyUrl = "";
    }
  } else if (wasSaved(AI_PROXY_URL_SECRET)) {
    saved.push(AI_PROXY_URL_SECRET);
  }

  const redactMap = (map: Record<string, string> | undefined, pathFor: (name: string) => string): Record<string, string> | undefined => {
    if (!map) return map;
    const next: Record<string, string> = {};
    for (const [name, value] of Object.entries(map)) {
      const path = pathFor(name);
      if ((value ?? "").length > 0 || wasSaved(path)) saved.push(path);
      next[name] = "";
    }
    return next;
  };

  const redacted: T = {
    ...config,
    apiKey: "",
    proxyUrl,
    customHeaders: redactMap(config.customHeaders, aiHeaderSecretPath),
    savedSecrets: saved,
  };
  for (const field of AI_CLI_ENV_FIELDS) {
    const map = config[field] as Record<string, string> | undefined;
    if (map) (redacted as AiConfig)[field] = redactMap(map, (name) => aiEnvSecretPath(field, name));
  }
  delete (redacted as AiConfig).clearedSecrets;
  return redacted;
}
