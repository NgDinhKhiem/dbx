export interface DiscoverErrorInfo {
  /** Short reason, e.g. `query_shard_exception: Failed to parse query [level:(WARN]`. */
  message: string;
  /** Deeper cause (e.g. the Lucene parse error) when available. */
  detail?: string;
  status?: number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() ? value : undefined;
}

function describeCause(cause: unknown): { message?: string; detail?: string } {
  if (!isRecord(cause)) return {};
  const type = asString(cause.type);
  const reason = asString(cause.reason);
  const message = type && reason ? `${type}: ${reason}` : (reason ?? type);
  const nested = isRecord(cause.caused_by) ? describeCause(cause.caused_by) : {};
  return { message, detail: nested.message };
}

/**
 * Extract a readable error from an Elasticsearch/OpenSearch error body
 * (search errors with `failed_shards`/`root_cause`, SQL/PPL plugin errors
 * with `reason`/`details`, or plain text).
 */
export function extractErrorInfo(body: string, status?: number): DiscoverErrorInfo {
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    const text = body.trim();
    return { message: text ? text.slice(0, 2000) : status ? `HTTP ${status}` : "Request failed", status };
  }
  if (isRecord(parsed)) {
    const error = parsed.error;
    if (typeof error === "string") return { message: error, status };
    if (isRecord(error)) {
      const failedShards = Array.isArray(error.failed_shards) ? error.failed_shards : [];
      const firstShard = failedShards.find((shard) => isRecord(shard) && isRecord(shard.reason));
      if (isRecord(firstShard)) {
        const described = describeCause(firstShard.reason);
        if (described.message) return { message: described.message, detail: described.detail, status };
      }
      const rootCause = Array.isArray(error.root_cause) ? error.root_cause[0] : undefined;
      if (isRecord(rootCause)) {
        const described = describeCause(rootCause);
        const top = describeCause(error);
        if (described.message) return { message: described.message, detail: top.detail ?? (top.message !== described.message ? top.message : undefined), status };
      }
      const described = describeCause(error);
      const details = asString(error.details);
      if (described.message) return { message: described.message, detail: details ?? described.detail, status };
    }
    const message = asString(parsed.message) ?? asString(parsed.reason);
    if (message) return { message, status };
  }
  return { message: status ? `HTTP ${status}` : "Request failed", detail: body.slice(0, 2000), status };
}

export function errorFromUnknown(error: unknown): DiscoverErrorInfo {
  if (error instanceof Error) return { message: error.message };
  if (typeof error === "string") return { message: error };
  if (isRecord(error) && typeof error.message === "string") return { message: error.message };
  return { message: String(error) };
}
