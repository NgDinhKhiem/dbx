import { elasticsearchRawRequest } from "@/lib/backend/api";
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

/** Detect OpenSearch vs Elasticsearch from `GET /` (`version.distribution`). */
export async function detectDistribution(connectionId: string): Promise<ClusterDistribution> {
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
