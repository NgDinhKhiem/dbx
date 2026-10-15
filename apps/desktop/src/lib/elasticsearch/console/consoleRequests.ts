import { formatJsonSource } from "@/lib/common/safeJsonFormat";
import { elasticsearchRestRequestRanges } from "@/lib/sql/sqlStatementRanges";
import { isElasticsearchCompatibleDatabaseType, type DatabaseType } from "@/types/database";

export type ConsoleHttpMethod = "GET" | "POST" | "PUT" | "DELETE" | "HEAD" | "PATCH";

/** One Kibana-console request found in the editor document. */
export interface ConsoleRequest {
  /** Document offset of the request line. */
  from: number;
  /** Document offset just after the request's last non-blank character. */
  to: number;
  /** The request text as written (request line plus body). */
  text: string;
  method: ConsoleHttpMethod;
  /** Path as written on the request line (may lack the leading slash). */
  rawPath: string;
  /** Normalized path with a leading slash, query string included. */
  path: string;
  /** Body ready to send: comments stripped, NDJSON compacted. */
  body?: string;
}

const REQUEST_LINE = /^\s*(GET|POST|PUT|DELETE|HEAD|PATCH)\s+(\S+)[^\S\n]*/i;
const NDJSON_ENDPOINTS = new Set(["_bulk", "_msearch"]);

export const DEFAULT_CONSOLE_TEXT = `# Welcome to the Dev Tools console.
# Put the cursor on a request and press Cmd/Ctrl+Enter to run it.
GET _search
{
  "query": {
    "match_all": {}
  }
}

GET _cat/indices?v
`;

function consoleDatabaseType(databaseType?: DatabaseType): DatabaseType {
  return isElasticsearchCompatibleDatabaseType(databaseType) ? (databaseType as DatabaseType) : "elasticsearch";
}

export function normalizeConsolePath(rawPath: string): string {
  return rawPath.startsWith("/") ? rawPath : `/${rawPath}`;
}

/** Path segments without the query string, e.g. `/logs/_bulk?refresh` → ["logs", "_bulk"]. */
export function consolePathSegments(path: string): string[] {
  return (path.split("?", 1)[0] ?? "").split("/").filter(Boolean);
}

export function isNdjsonConsolePath(path: string): boolean {
  return consolePathSegments(path).some((segment) => NDJSON_ENDPOINTS.has(segment.toLowerCase()));
}

/**
 * Kibana accepts `"""multi-line strings"""` (handy for scripts and SQL); the
 * server only understands JSON strings, so they are escaped before sending.
 */
export function convertTripleQuotedStrings(body: string): string {
  return body.replace(/"""([\s\S]*?)"""/g, (_match, content: string) => JSON.stringify(content));
}

function isCommentLine(line: string): boolean {
  const trimmed = line.trimStart();
  return trimmed.startsWith("#") || trimmed.startsWith("//");
}

export function stripConsoleCommentLines(body: string): string {
  return body
    .split("\n")
    .filter((line) => !isCommentLine(line))
    .join("\n");
}

/**
 * Splits concatenated JSON values (NDJSON, possibly pretty-printed across
 * several lines) into one source slice per top-level value.
 */
export function splitJsonValues(source: string): string[] {
  const values: string[] = [];
  let index = 0;
  while (index < source.length) {
    while (index < source.length && /\s/.test(source[index] ?? "")) index += 1;
    if (index >= source.length) break;
    const start = index;
    const opener = source[index];
    if (opener !== "{" && opener !== "[") {
      // A primitive or malformed value: keep the rest of the line as one entry.
      const newline = source.indexOf("\n", index);
      index = newline < 0 ? source.length : newline;
      values.push(source.slice(start, index).trim());
      continue;
    }
    let depth = 0;
    let inString = false;
    for (; index < source.length; index += 1) {
      const char = source[index];
      if (inString) {
        if (char === "\\") index += 1;
        else if (char === '"') inString = false;
        continue;
      }
      if (char === '"') inString = true;
      else if (char === "{" || char === "[") depth += 1;
      else if (char === "}" || char === "]") {
        depth -= 1;
        if (depth === 0) {
          index += 1;
          break;
        }
      }
    }
    values.push(source.slice(start, index).trim());
  }
  return values.filter(Boolean);
}

function compactJson(value: string): string {
  try {
    return formatJsonSource(value);
  } catch {
    return value.replace(/\s*\n\s*/g, " ");
  }
}

/** Builds the body sent to the cluster from the text written under a request line. */
export function buildConsoleRequestBody(rawBody: string, path: string): string | undefined {
  const body = stripConsoleCommentLines(convertTripleQuotedStrings(rawBody)).trim();
  if (!body) return undefined;
  if (!isNdjsonConsolePath(path)) return body;
  return `${splitJsonValues(body).map(compactJson).join("\n")}\n`;
}

/** Parses a single request (`METHOD path` line plus optional body). */
export function parseConsoleRequestText(text: string): Pick<ConsoleRequest, "method" | "rawPath" | "path" | "body"> | null {
  const match = REQUEST_LINE.exec(text);
  if (!match) return null;
  const method = match[1].toUpperCase() as ConsoleHttpMethod;
  const rawPath = match[2];
  const path = normalizeConsolePath(rawPath);
  const body = buildConsoleRequestBody(text.slice(match[0].length), path);
  return body === undefined ? { method, rawPath, path } : { method, rawPath, path, body };
}

/** Finds every request in the console document. */
export function parseConsoleRequests(text: string, databaseType?: DatabaseType): ConsoleRequest[] {
  const requests: ConsoleRequest[] = [];
  for (const range of elasticsearchRestRequestRanges(text, consoleDatabaseType(databaseType))) {
    const parsed = parseConsoleRequestText(range.sql);
    if (!parsed) continue;
    requests.push({ from: range.from, to: range.to, text: range.sql, ...parsed });
  }
  return requests;
}

/**
 * The requests a run targets: with an empty selection, the request under the
 * cursor (a cursor in the gap after a request still belongs to it, like
 * Dashboards); with a selection, every request the selection touches.
 */
export function consoleRequestsForSelection(requests: readonly ConsoleRequest[], from: number, to: number = from): ConsoleRequest[] {
  if (!requests.length) return [];
  const start = Math.min(from, to);
  const end = Math.max(from, to);
  if (start === end) {
    let current: ConsoleRequest | undefined;
    for (const request of requests) {
      if (request.from <= start) current = request;
      else break;
    }
    return current ? [current] : [];
  }
  const touched = requests.filter((request, index) => {
    const nextFrom = requests[index + 1]?.from ?? Number.POSITIVE_INFINITY;
    return request.from < end && nextFrom > start;
  });
  return touched;
}

/** Display label used in response headers and history, e.g. `GET _cat/indices?v`. */
export function consoleRequestLabel(request: Pick<ConsoleRequest, "method" | "rawPath">): string {
  return `${request.method} ${request.rawPath}`;
}

export type ConsoleFormatResult = { ok: true; text: string } | { ok: false; reason: "invalid-json" | "unsupported" };

/**
 * Auto-indent like Dashboards: pretty-print the JSON body, or collapse it back
 * to one line when it is already pretty. NDJSON bodies keep one document per
 * line. Comment lines and triple-quoted strings are left untouched by refusing
 * to format them rather than silently rewriting them.
 */
export function autoIndentConsoleRequest(text: string, indent = 2): ConsoleFormatResult {
  const match = REQUEST_LINE.exec(text);
  if (!match) return { ok: false, reason: "unsupported" };
  const requestLine = `${match[1].toUpperCase()} ${match[2]}`;
  const rawBody = text.slice(match[0].length);
  const body = rawBody.trim();
  if (!body) return { ok: true, text: requestLine };
  if (body.includes('"""') || body.split("\n").some(isCommentLine)) return { ok: false, reason: "unsupported" };

  try {
    if (isNdjsonConsolePath(match[2])) {
      const lines = splitJsonValues(body).map((value) => formatJsonSource(value));
      return { ok: true, text: `${requestLine}\n${lines.join("\n")}` };
    }
    const pretty = formatJsonSource(body, indent);
    const formatted = pretty === body ? formatJsonSource(body) : pretty;
    return { ok: true, text: `${requestLine}\n${formatted}` };
  } catch {
    return { ok: false, reason: "invalid-json" };
  }
}
