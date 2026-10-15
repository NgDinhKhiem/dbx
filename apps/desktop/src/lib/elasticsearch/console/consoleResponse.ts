import { formatJsonSource } from "@/lib/common/safeJsonFormat";

/** Bodies above this size are shown as returned instead of being re-indented. */
export const CONSOLE_RESPONSE_FORMAT_MAX_LENGTH = 8 * 1024 * 1024;

export type ConsoleResponseLanguage = "json" | "console" | "text";

export type ConsoleStatusTone = "success" | "redirect" | "client-error" | "server-error" | "error";

export interface ConsoleResponseEntry {
  /** Request line label, e.g. `GET _search`. */
  label: string;
  status?: number;
  tookMs?: number;
  body: string;
  /** Transport or client-side failure (no HTTP response). */
  error?: string;
  /** Error was a read-only connection rejecting a write. */
  readOnly?: boolean;
}

export interface FormattedConsoleBody {
  text: string;
  isJson: boolean;
}

const STATUS_TEXT: Record<number, string> = {
  200: "OK",
  201: "Created",
  202: "Accepted",
  204: "No Content",
  301: "Moved Permanently",
  302: "Found",
  304: "Not Modified",
  400: "Bad Request",
  401: "Unauthorized",
  403: "Forbidden",
  404: "Not Found",
  405: "Method Not Allowed",
  406: "Not Acceptable",
  408: "Request Timeout",
  409: "Conflict",
  413: "Payload Too Large",
  415: "Unsupported Media Type",
  429: "Too Many Requests",
  500: "Internal Server Error",
  502: "Bad Gateway",
  503: "Service Unavailable",
  504: "Gateway Timeout",
};

export function consoleStatusText(status: number): string {
  return STATUS_TEXT[status] ?? "";
}

export function consoleStatusTone(status: number | undefined): ConsoleStatusTone {
  if (status === undefined) return "error";
  if (status >= 200 && status < 300) return "success";
  if (status >= 300 && status < 400) return "redirect";
  if (status >= 400 && status < 500) return "client-error";
  return "server-error";
}

/** Pretty-prints JSON bodies (lossless: big numbers and key order are kept); other text is returned as is. */
export function formatConsoleResponseBody(body: string): FormattedConsoleBody {
  const trimmed = body.trim();
  if (!trimmed || !(trimmed.startsWith("{") || trimmed.startsWith("["))) return { text: body, isJson: false };
  if (trimmed.length > CONSOLE_RESPONSE_FORMAT_MAX_LENGTH) return { text: body, isJson: true };
  try {
    return { text: formatJsonSource(trimmed, 2), isJson: true };
  } catch {
    return { text: body, isJson: false };
  }
}

export function consoleResponseHeader(entry: ConsoleResponseEntry): string {
  if (entry.status === undefined) return `# ${entry.label}  error`;
  const text = consoleStatusText(entry.status);
  return `# ${entry.label}  ${entry.status}${text ? ` ${text}` : ""}`;
}

function entryBodyText(entry: ConsoleResponseEntry): FormattedConsoleBody {
  if (entry.error !== undefined) return { text: entry.error, isJson: false };
  return formatConsoleResponseBody(entry.body);
}

/**
 * Text shown in the response viewer. A single response is shown alone; several
 * responses are separated by `# METHOD path  status` headers like Dashboards.
 */
export function consoleResponseDocument(entries: readonly ConsoleResponseEntry[]): FormattedConsoleBody {
  if (entries.length === 0) return { text: "", isJson: false };
  if (entries.length === 1) return entryBodyText(entries[0]);
  const text = entries.map((entry) => `${consoleResponseHeader(entry)}\n${entryBodyText(entry).text.replace(/\n+$/, "")}`).join("\n\n");
  return { text: `${text}\n`, isJson: false };
}

export function isReadOnlyConsoleError(message: string): boolean {
  return /READ_ONLY/.test(message);
}
