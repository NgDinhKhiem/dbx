import { elasticsearchRawRequest } from "@/lib/backend/api";
import type { ConsoleHttpMethod } from "@/lib/elasticsearch/console/consoleRequests";

export interface ConsoleRawRequest {
  method: ConsoleHttpMethod;
  path: string;
  body?: string;
}

export interface ConsoleRawResponse {
  status: number;
  body: string;
  tookMs: number;
}

/**
 * Sends one raw REST request through the backend. Non-2xx statuses come back
 * as data; transport failures (and read-only rejections) throw.
 */
export function sendConsoleRequest(connectionId: string, request: ConsoleRawRequest): Promise<ConsoleRawResponse> {
  return elasticsearchRawRequest(connectionId, request.body === undefined ? { method: request.method, path: request.path } : { method: request.method, path: request.path, body: request.body });
}
