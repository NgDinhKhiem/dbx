import { describe, expect, it } from "vitest";
import { DEFAULT_CONSOLE_TEXT, autoIndentConsoleRequest, buildConsoleRequestBody, consoleRequestLabel, consoleRequestsForSelection, convertTripleQuotedStrings, isNdjsonConsolePath, parseConsoleRequestText, parseConsoleRequests, splitJsonValues } from "@/lib/elasticsearch/console/consoleRequests";

const DOC = `# comment before
GET _search
{
  "query": { "match_all": {} }
}

// another comment
GET _cat/indices?v

POST /logs/_bulk
{ "index": { "_id": "1" } }
{ "msg": "a" }
`;

describe("parseConsoleRequests", () => {
  it("splits a document into requests with method, path and body", () => {
    const requests = parseConsoleRequests(DOC);
    expect(requests.map((request) => [request.method, request.path])).toEqual([
      ["GET", "/_search"],
      ["GET", "/_cat/indices?v"],
      ["POST", "/logs/_bulk"],
    ]);
    expect(requests[0].body).toBe('{\n  "query": { "match_all": {} }\n}');
    expect(requests[1].body).toBeUndefined();
    expect(requests[0].text.startsWith("GET _search")).toBe(true);
    expect(DOC.slice(requests[1].from, requests[1].to)).toBe("GET _cat/indices?v");
  });

  it("does not include the comment preceding the next request in the previous body", () => {
    const [first] = parseConsoleRequests(DOC);
    expect(first.text).not.toContain("another comment");
  });

  it("compacts NDJSON bodies to one document per line with a trailing newline", () => {
    const bulk = parseConsoleRequests(DOC)[2];
    expect(bulk.body).toBe('{"index":{"_id":"1"}}\n{"msg":"a"}\n');
  });

  it("parses the default console text", () => {
    expect(parseConsoleRequests(DEFAULT_CONSOLE_TEXT).map(consoleRequestLabel)).toEqual(["GET _search", "GET _cat/indices?v"]);
  });

  it("returns nothing for text without a request line", () => {
    expect(parseConsoleRequests("hello world")).toEqual([]);
  });
});

describe("request bodies", () => {
  it("strips comment lines and converts triple-quoted strings", () => {
    const body = buildConsoleRequestBody('\n{\n  # note\n  "script": """\n  // painless comment\n  ctx._source.a = 1\n  """\n}', "/i/_update/1");
    expect(JSON.parse(body ?? "")).toEqual({ script: "\n  // painless comment\n  ctx._source.a = 1\n  " });
  });

  it("detects NDJSON endpoints", () => {
    expect(isNdjsonConsolePath("/_bulk")).toBe(true);
    expect(isNdjsonConsolePath("/idx/_msearch?x=1")).toBe(true);
    expect(isNdjsonConsolePath("/idx/_search")).toBe(false);
  });

  it("splits pretty-printed concatenated JSON values", () => {
    expect(splitJsonValues('{\n "a": "}"\n}\n{"b": [1,\n2]}\n')).toEqual(['{\n "a": "}"\n}', '{"b": [1,\n2]}']);
  });

  it("escapes triple-quoted content as a JSON string", () => {
    expect(convertTripleQuotedStrings('{"q": """say "hi"\n"""}')).toBe('{"q": "say \\"hi\\"\\n"}');
  });

  it("parses a single request with a lowercase method and body on the request line", () => {
    expect(parseConsoleRequestText('put my-index\n{"settings": {}}')).toEqual({ method: "PUT", rawPath: "my-index", path: "/my-index", body: '{"settings": {}}' });
  });
});

describe("consoleRequestsForSelection", () => {
  const requests = parseConsoleRequests(DOC);

  it("returns the request under the cursor", () => {
    const cursor = DOC.indexOf("match_all");
    expect(consoleRequestsForSelection(requests, cursor).map(consoleRequestLabel)).toEqual(["GET _search"]);
  });

  it("keeps the previous request active in the gap before the next one", () => {
    const cursor = DOC.indexOf("// another");
    expect(consoleRequestsForSelection(requests, cursor).map(consoleRequestLabel)).toEqual(["GET _search"]);
  });

  it("returns nothing when the cursor is before the first request", () => {
    expect(consoleRequestsForSelection(requests, 0)).toEqual([]);
  });

  it("returns every request touched by a selection", () => {
    const from = DOC.indexOf("match_all");
    const to = DOC.indexOf("_bulk");
    expect(consoleRequestsForSelection(requests, from, to).map(consoleRequestLabel)).toEqual(["GET _search", "GET _cat/indices?v", "POST /logs/_bulk"]);
  });
});

describe("autoIndentConsoleRequest", () => {
  it("pretty-prints a compact body and collapses an already pretty one", () => {
    const pretty = autoIndentConsoleRequest('get _search\n{"query":{"match_all":{}}}');
    expect(pretty).toEqual({ ok: true, text: 'GET _search\n{\n  "query": {\n    "match_all": {}\n  }\n}' });
    if (!pretty.ok) throw new Error("expected ok");
    expect(autoIndentConsoleRequest(pretty.text)).toEqual({ ok: true, text: 'GET _search\n{"query":{"match_all":{}}}' });
  });

  it("keeps one document per line for NDJSON", () => {
    expect(autoIndentConsoleRequest('POST _bulk\n{ "index": {} }\n{\n "a": 1\n}')).toEqual({ ok: true, text: 'POST _bulk\n{"index":{}}\n{"a":1}' });
  });

  it("reports invalid JSON and refuses to drop comments", () => {
    expect(autoIndentConsoleRequest("GET _search\n{ nope")).toEqual({ ok: false, reason: "invalid-json" });
    expect(autoIndentConsoleRequest("GET _search\n# c\n{}")).toEqual({ ok: false, reason: "unsupported" });
  });
});
