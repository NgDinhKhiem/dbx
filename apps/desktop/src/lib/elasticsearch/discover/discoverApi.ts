import { elasticsearchClusterInfo, elasticsearchRawRequest } from "@/lib/backend/api";
import type { ClusterDistribution } from "./types";

export type DiscoverHttpMethod = "GET" | "POST" | "PUT" | "DELETE" | "HEAD" | "PATCH";

export interface DiscoverRawRequest {
  method: DiscoverHttpMethod;
  path: string;
  body?: string;
}

export interface DiscoverRawResponse {
  status: number;
  body: string;
  tookMs: number;
}

/** Thin wrapper over the backend raw request so tests can mock one module. */
export function discoverRequest(connectionId: string, request: DiscoverRawRequest): Promise<DiscoverRawResponse> {
  return elasticsearchRawRequest(connectionId, request);
}

export function isSuccessStatus(status: number): boolean {
  return status >= 200 && status < 300;
}

/**
 * Detect OpenSearch vs Elasticsearch. The backend caches the distribution learned
 * by the connect-time `GET /` check, so this normally costs no cluster round trip;
 * a raw `GET /` is only the fallback (older backends, restricted accounts).
 */
export async function detectDistribution(connectionId: string): Promise<ClusterDistribution> {
  try {
    const info = await elasticsearchClusterInfo(connectionId);
    if (info?.distribution) return info.distribution.toLowerCase() === "opensearch" ? "opensearch" : "elasticsearch";
  } catch {
    // Fall back to reading the cluster root directly.
  }
  try {
    const response = await discoverRequest(connectionId, { method: "GET", path: "/" });
    if (!isSuccessStatus(response.status)) return "elasticsearch";
    const parsed = JSON.parse(response.body) as { version?: { distribution?: string } };
    return parsed.version?.distribution === "opensearch" ? "opensearch" : "elasticsearch";
  } catch {
    return "elasticsearch";
  }
}

/** Encode an index pattern for a URL path segment, keeping `*` and `,` readable. */
export function encodeIndexPattern(pattern: string): string {
  return pattern
    .split(",")
    .map((part) => encodeURIComponent(part.trim()).replace(/%2A/gi, "*"))
    .join(",");
}
