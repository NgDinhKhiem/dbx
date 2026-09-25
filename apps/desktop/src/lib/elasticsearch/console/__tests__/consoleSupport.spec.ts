import { describe, expect, it } from "vitest";
import { consoleBaseUrl, consoleRequestToCurl } from "@/lib/elasticsearch/console/consoleCurl";
import { CONSOLE_HISTORY_LIMIT, CONSOLE_HISTORY_MAX_TEXT_LENGTH, addConsoleHistoryEntries, createConsoleHistoryEntry, loadConsoleHistory, recordConsoleHistory } from "@/lib/elasticsearch/console/consoleHistory";
import { findMatchingJsonBracket } from "@/lib/elasticsearch/console/consoleLanguage";
import { parseConsoleRequests } from "@/lib/elasticsearch/console/consoleRequests";
import { consoleResponseDocument, consoleStatusTone, formatConsoleResponseBody, isReadOnlyConsoleError } from "@/lib/elasticsearch/console/consoleResponse";
import { assessConsoleRequests, consoleRequestIndices, consoleRequestRisk } from "@/lib/elasticsearch/console/consoleSafety";
import { clampConsoleSplitRatio } from "@/lib/elasticsearch/console/consoleSplit";
import type { ConnectionConfig } from "@/types/database";

function memoryStorage() {
  const data = new Map<string, string>();
  return {
    data,
    getItem: (key: string) => data.get(key) ?? null,
    setItem: (key: string, value: string) => void data.set(key, value),
  };
}

describe("consoleCurl", () => {
  it("builds the base URL from host, port and TLS flag", () => {
    expect(consoleBaseUrl({ host: "localhost", port: 9202 })).toBe("http://localhost:9202");
    expect(consoleBaseUrl({ host: "es.example.com", port: 443, ssl: true })).toBe("https://es.example.com:443");
    expect(consoleBaseUrl({ host: "https://es.example.com/", port: 9200 })).toBe("https://es.example.com:9200");
    expect(consoleBaseUrl({ host: "http://es:9300", port: 9200 })).toBe("http://es:9300");
  });

  it("renders requests like Dashboards and escapes single quotes", () => {
    const [search, bulk] = parseConsoleRequests(`GET _search\n{"q": "it's"}\n\nPOST _bulk\n{"index":{}}\n{"a":1}\n`);
    expect(consoleRequestToCurl(search, "http://localhost:9200")).toBe(`curl -XGET "http://localhost:9200/_search" -H 'Content-Type: application/json' -d '\n{"q": "it'\\''s"}\n'`);
    expect(consoleRequestToCurl(bulk, "http://h:1")).toBe(`curl -XPOST "http://h:1/_bulk" -H 'Content-Type: application/x-ndjson' --data-binary '\n{"index":{}}\n{"a":1}\n'`);
    expect(consoleRequestToCurl({ method: "HEAD", path: "/idx" }, "http://h:1/")).toBe('curl -XHEAD "http://h:1/idx"');
  });
});

describe("consoleHistory", () => {
  it("keeps newest first, de-duplicates and caps at the limit", () => {
    let entries = addConsoleHistoryEntries([], [createConsoleHistoryEntry("GET a", "GET a", 1), createConsoleHistoryEntry("GET b", "GET b", 2)]);
    entries = addConsoleHistoryEntries(entries, [createConsoleHistoryEntry("GET a", "GET a", 3)]);
    expect(entries.map((entry) => [entry.label, entry.executedAt])).toEqual([
      ["GET a", 3],
      ["GET b", 2],
    ]);
    const many = Array.from({ length: CONSOLE_HISTORY_LIMIT + 5 }, (_, index) => createConsoleHistoryEntry(`GET ${index}`, `GET ${index}`, index));
    expect(addConsoleHistoryEntries([], many)).toHaveLength(CONSOLE_HISTORY_LIMIT);
  });

  it("drops oversized bodies and stores per connection", () => {
    const storage = memoryStorage();
    const huge = `POST _bulk\n${"x".repeat(CONSOLE_HISTORY_MAX_TEXT_LENGTH)}`;
    recordConsoleHistory("c1", [createConsoleHistoryEntry("POST _bulk", huge, 5)], storage);
    expect(loadConsoleHistory("c1", storage)).toEqual([{ label: "POST _bulk", text: "POST _bulk", executedAt: 5, truncated: true }]);
    expect(loadConsoleHistory("c2", storage)).toEqual([]);
  });

  it("survives broken or unavailable storage", () => {
    const storage = memoryStorage();
    storage.setItem("dbx.esConsole.history.v1.c1", "{not json");
    expect(loadConsoleHistory("c1", storage)).toEqual([]);
    const throwing = {
      getItem: () => {
        throw new Error("denied");
      },
      setItem: () => {
        throw new Error("denied");
      },
    };
    expect(loadConsoleHistory("c1", throwing)).toEqual([]);
    expect(() => recordConsoleHistory("c1", [createConsoleHistoryEntry("GET a", "GET a")], throwing)).not.toThrow();
  });
});

describe("consoleResponse", () => {
  it("pretty-prints JSON without losing big numbers and leaves text alone", () => {
    expect(formatConsoleResponseBody('{"id":12345678901234567890}')).toEqual({ text: '{\n  "id": 12345678901234567890\n}', isJson: true });
    expect(formatConsoleResponseBody("health status index\ngreen open logs\n")).toEqual({ text: "health status index\ngreen open logs\n", isJson: false });
  });

  it("maps status codes to tones", () => {
    expect([200, 301, 404, 503, undefined].map(consoleStatusTone)).toEqual(["success", "redirect", "client-error", "server-error", "error"]);
  });

  it("separates several responses with request headers", () => {
    const document = consoleResponseDocument([
      { label: "GET _cat/health", status: 200, tookMs: 3, body: "green\n" },
      { label: "GET nope/_search", status: 404, tookMs: 1, body: '{"error":"x"}' },
      { label: "DELETE x", body: "", error: "boom" },
    ]);
    expect(document.isJson).toBe(false);
    expect(document.text).toBe('# GET _cat/health  200 OK\ngreen\n\n# GET nope/_search  404 Not Found\n{\n  "error": "x"\n}\n\n# DELETE x  error\nboom\n');
  });

  it("detects read-only rejections", () => {
    expect(isReadOnlyConsoleError("READ_ONLY: connection 'x' has read-only protection")).toBe(true);
    expect(isReadOnlyConsoleError("timeout")).toBe(false);
  });
});

describe("consoleSafety", () => {
  const requests = parseConsoleRequests("GET _search\n\nPOST logs/_search\n{}\n\nDELETE logs\n\nPOST logs/_close\n\nPUT logs/_doc/1\n{}\n\nPOST logs/_delete_by_query\n{}\n");

  it("classifies with the shared Elasticsearch risk rules", () => {
    expect(requests.map(consoleRequestRisk)).toEqual(["read", "read", "dangerous", "dangerous", "write", "dangerous"]);
  });

  it("only flags writes and production context", () => {
    expect(assessConsoleRequests(requests.slice(0, 2), { db_type: "elasticsearch", is_production: true }).risky).toEqual([]);
    const assessment = assessConsoleRequests(requests, { db_type: "elasticsearch", is_production: true });
    expect(assessment.risky).toHaveLength(4);
    expect(assessment.production.active).toBe(true);
    expect(assessConsoleRequests(requests, { db_type: "elasticsearch" }).production.active).toBe(false);
  });

  it("treats writes to marked production indices as production", () => {
    const connection = { db_type: "elasticsearch", production_databases: ["logs"] } as Pick<ConnectionConfig, "db_type" | "production_databases">;
    expect(assessConsoleRequests(requests, connection).production).toEqual({ active: true, databases: ["logs"] });
    expect(consoleRequestIndices("/a,b%2Dc/_doc/1")).toEqual(["a", "b-c"]);
    expect(consoleRequestIndices("/_bulk")).toEqual([]);
  });
});

describe("helpers", () => {
  it("matches JSON brackets while skipping strings", () => {
    const text = '{"a": "}", "b": [1, {"c": 2}]}';
    expect(findMatchingJsonBracket(text, 0)).toBe(text.length - 1);
    expect(findMatchingJsonBracket("{ unbalanced", 0)).toBe(-1);
  });

  it("clamps the split ratio", () => {
    expect(clampConsoleSplitRatio(0.05)).toBe(0.2);
    expect(clampConsoleSplitRatio(0.95)).toBe(0.8);
    expect(clampConsoleSplitRatio(Number.NaN)).toBe(0.5);
  });
});
