import type { ConsoleRequest } from "@/lib/elasticsearch/console/consoleRequests";
import { isNdjsonConsolePath } from "@/lib/elasticsearch/console/consoleRequests";

export interface ConsoleBaseUrlSource {
  host?: string;
  port?: number;
  ssl?: boolean;
}

/**
 * Best-effort cluster URL for "Copy as cURL". Credentials are never included;
 * the user adds `-u` themselves when the cluster needs it.
 */
export function consoleBaseUrl(connection: ConsoleBaseUrlSource | undefined): string {
  const host = String(connection?.host ?? "").trim() || "localhost";
  const port = connection?.port;
  if (/^https?:\/\//i.test(host)) {
    const url = host.replace(/\/+$/, "");
    const hasPort = /^https?:\/\/(?:\[[^\]]+\]|[^/:]+):\d+/i.test(url);
    return hasPort || !port ? url : url.replace(/^(https?:\/\/(?:\[[^\]]+\]|[^/:]+))/i, `$1:${port}`);
  }
  const scheme = connection?.ssl ? "https" : "http";
  return port ? `${scheme}://${host}:${port}` : `${scheme}://${host}`;
}

function shellSingleQuote(value: string): string {
  return value.replace(/'/g, "'\\''");
}

/** Renders a request the way Dashboards' "Copy as cURL" does. */
export function consoleRequestToCurl(request: Pick<ConsoleRequest, "method" | "path" | "body">, baseUrl: string): string {
  const url = `${baseUrl.replace(/\/+$/, "")}${request.path}`;
  const head = `curl -X${request.method} "${url.replace(/"/g, '\\"')}"`;
  if (request.body === undefined) return head;
  const contentType = isNdjsonConsolePath(request.path) ? "application/x-ndjson" : "application/json";
  const flag = isNdjsonConsolePath(request.path) ? "--data-binary" : "-d";
  return `${head} -H 'Content-Type: ${contentType}' ${flag} '\n${shellSingleQuote(request.body.replace(/\n$/, ""))}\n'`;
}
