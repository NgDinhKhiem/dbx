import { describe, expect, it } from "vitest";
import { DqlSyntaxError, dqlToDsl, escapeLucene, formatDqlErrorPointer, parseDql, tokenizeDql } from "../dql";

function syntaxError(query: string): DqlSyntaxError {
  try {
    parseDql(query);
  } catch (error) {
    if (error instanceof DqlSyntaxError) return error;
    throw error;
  }
  throw new Error(`expected a syntax error for ${query}`);
}

describe("tokenizeDql", () => {
  it("splits fields, operators, keywords and values with positions", () => {
    const tokens = tokenizeDql('level:ERROR AND not (a >= 10 or msg:"x y")');
    expect(tokens.map((token) => token.type)).toEqual(["word", "colon", "word", "and", "not", "lparen", "word", "range", "word", "or", "word", "colon", "quoted", "rparen", "eof"]);
    expect(tokens[0]).toMatchObject({ start: 0, end: 5, value: "level" });
    expect(tokens[7]).toMatchObject({ value: ">=", start: 23 });
    expect(tokens[12]).toMatchObject({ value: "x y" });
  });

  it("recognizes keywords case-insensitively but not when escaped", () => {
    expect(tokenizeDql("a Or b AND c NoT d").map((token) => token.type)).toEqual(["word", "or", "word", "and", "word", "not", "word", "eof"]);
    const escaped = tokenizeDql("\\and");
    expect(escaped[0]).toMatchObject({ type: "word", value: "and" });
  });

  it("unescapes special characters and tracks wildcards", () => {
    const [token] = tokenizeDql("url\\:http\\://x\\*y*");
    expect(token.value).toBe("url:http://x*y*");
    expect(token.wildcard).toBe(true);
    expect(token.queryString).toBe("url\\:http\\:\\/\\/x\\*y*");
  });

  it("handles escaped quotes inside phrases", () => {
    const [token] = tokenizeDql('"say \\"hi\\""');
    expect(token).toMatchObject({ type: "quoted", value: 'say "hi"' });
  });
});

describe("parseDql", () => {
  it("returns matchAll for empty input", () => {
    expect(parseDql("   ")).toEqual({ type: "matchAll" });
  });

  it("gives AND precedence over OR", () => {
    expect(parseDql("a:1 or b:2 and c:3")).toMatchObject({
      type: "or",
      children: [
        { type: "field", field: "a" },
        { type: "and", children: [{ field: "b" }, { field: "c" }] },
      ],
    });
  });

  it("respects parentheses", () => {
    expect(parseDql("(a:1 or b:2) and c:3")).toMatchObject({
      type: "and",
      children: [{ type: "or" }, { type: "field", field: "c" }],
    });
  });

  it("binds NOT tighter than AND", () => {
    expect(parseDql("not a:1 and b:2")).toMatchObject({ type: "and", children: [{ type: "not", child: { field: "a" } }, { field: "b" }] });
  });

  it("joins adjacent unquoted words into one value", () => {
    expect(parseDql("hello big world")).toEqual({ type: "free", value: { type: "literal", value: "hello big world", queryString: "hello\\ big\\ world", quoted: false, wildcard: false } });
  });

  it("stops a value before the next field expression and ORs adjacent expressions", () => {
    expect(parseDql("level:WARN service.name:api")).toMatchObject({
      type: "or",
      children: [
        { type: "field", field: "level", value: { value: "WARN" } },
        { type: "field", field: "service.name", value: { value: "api" } },
      ],
    });
  });

  it("parses value lists", () => {
    expect(parseDql("level:(ERROR or WARN and not DEBUG)")).toMatchObject({
      type: "field",
      field: "level",
      value: { type: "or", children: [{ value: "ERROR" }, { type: "and", children: [{ value: "WARN" }, { type: "not", child: { value: "DEBUG" } }] }] },
    });
  });

  it("parses range operators", () => {
    expect(parseDql("severity >= 40")).toEqual({ type: "range", field: "severity", operator: "gte", value: expect.objectContaining({ value: "40" }) });
    expect(parseDql("severity<40")).toMatchObject({ operator: "lt" });
    expect(parseDql("severity > 40")).toMatchObject({ operator: "gt" });
    expect(parseDql('@timestamp <= "2026-09-23T10:00:00"')).toMatchObject({ operator: "lte", value: { value: "2026-09-23T10:00:00", quoted: true } });
  });

  it("reports unterminated strings with their position", () => {
    const error = syntaxError('message:"abc');
    expect(error.position).toBe(8);
    expect(error.message).toBe("Unterminated quoted string at position 9");
    expect(formatDqlErrorPointer(error)).toBe('message:"abc\n        ^');
  });

  it("reports a missing closing parenthesis", () => {
    const error = syntaxError("(a:1 or b:2");
    expect(error.message).toContain('Expected ")" but found end of input');
    expect(error.position).toBe(11);
  });

  it("reports a missing value", () => {
    expect(syntaxError("level:").message).toContain("Expected a value but found end of input");
    expect(syntaxError("level: and x").message).toContain('Expected a value but found "and"');
    expect(syntaxError("severity >=").message).toContain("Expected a value");
  });

  it("reports dangling operators and stray parentheses", () => {
    expect(syntaxError("a:1 and").message).toContain("Expected a field, value or");
    expect(syntaxError("a:1)").message).toContain('Unexpected ")"');
    expect(syntaxError("()").message).toContain("Expected a query");
    expect(syntaxError("a\\").message).toContain("Dangling escape");
  });

  it("rejects nested field syntax clearly", () => {
    expect(syntaxError("items:{ name:x }").message).toContain("Nested field queries");
  });
});

describe("dqlToDsl", () => {
  it("translates field:value to match", () => {
    expect(dqlToDsl("level:ERROR")).toEqual({ match: { level: "ERROR" } });
  });

  it("translates quoted values to match_phrase", () => {
    expect(dqlToDsl('message:"connection refused"')).toEqual({ match_phrase: { message: "connection refused" } });
  });

  it("translates wildcards to a field-scoped query_string", () => {
    expect(dqlToDsl("service.name:aml-*")).toEqual({ query_string: { fields: ["service.name"], query: "aml\\-*" } });
  });

  it("translates field:* to exists", () => {
    expect(dqlToDsl("trace_id:*")).toEqual({ exists: { field: "trace_id" } });
    expect(dqlToDsl('trace_id:"*"')).toEqual({ match_phrase: { trace_id: "*" } });
  });

  it("translates and/or/not to bool clauses", () => {
    expect(dqlToDsl("level:ERROR and not service:api or is_error:true")).toEqual({
      bool: {
        should: [{ bool: { filter: [{ match: { level: "ERROR" } }, { bool: { must_not: { match: { service: "api" } } } }] } }, { match: { is_error: "true" } }],
        minimum_should_match: 1,
      },
    });
  });

  it("translates value lists", () => {
    expect(dqlToDsl("level:(ERROR or WARN)")).toEqual({ bool: { should: [{ match: { level: "ERROR" } }, { match: { level: "WARN" } }], minimum_should_match: 1 } });
    expect(dqlToDsl('message:("a b" and c)')).toEqual({ bool: { filter: [{ match_phrase: { message: "a b" } }, { match: { message: "c" } }] } });
  });

  it("translates ranges", () => {
    expect(dqlToDsl("severity >= 40 and http_status < 500")).toEqual({
      bool: { filter: [{ range: { severity: { gte: "40" } } }, { range: { http_status: { lt: "500" } } }] },
    });
  });

  it("translates free text to multi_match / query_string", () => {
    expect(dqlToDsl("timeout error")).toEqual({ multi_match: { type: "best_fields", query: "timeout error", lenient: true } });
    expect(dqlToDsl('"bean creation"')).toEqual({ multi_match: { type: "phrase", query: "bean creation", lenient: true } });
    expect(dqlToDsl("Bean*")).toEqual({ query_string: { query: "Bean*" } });
    expect(dqlToDsl("*")).toEqual({ match_all: {} });
    expect(dqlToDsl("")).toEqual({ match_all: {} });
  });

  it("keeps escaped characters literal", () => {
    expect(dqlToDsl("path:C\\:\\\\tmp")).toEqual({ match: { path: "C:\\tmp" } });
    expect(dqlToDsl("name:a\\*b")).toEqual({ match: { name: "a*b" } });
    expect(dqlToDsl("level:\\or")).toEqual({ match: { level: "or" } });
  });

  it("expands wildcard field names against known fields", () => {
    const fields = [
      { name: "kubernetes.pod", searchable: true },
      { name: "kubernetes.ns", searchable: true },
      { name: "level", searchable: true },
    ];
    expect(dqlToDsl("kubernetes.*:web", { fields })).toEqual({
      bool: { should: [{ match: { "kubernetes.pod": "web" } }, { match: { "kubernetes.ns": "web" } }], minimum_should_match: 1 },
    });
    expect(dqlToDsl("unknown.*:web", { fields })).toEqual({ multi_match: { query: "web", fields: ["unknown.*"], type: "best_fields", lenient: true } });
    expect(dqlToDsl("*:*")).toEqual({ match_all: {} });
  });

  it("combines nested parentheses and negated groups", () => {
    expect(dqlToDsl("not (level:ERROR or level:WARN) and service:(api or web)")).toEqual({
      bool: {
        filter: [{ bool: { must_not: { bool: { should: [{ match: { level: "ERROR" } }, { match: { level: "WARN" } }], minimum_should_match: 1 } } } }, { bool: { should: [{ match: { service: "api" } }, { match: { service: "web" } }], minimum_should_match: 1 } }],
      },
    });
  });
});

describe("escapeLucene", () => {
  it("escapes reserved characters and whitespace", () => {
    expect(escapeLucene('a+b (c) "d" e:f/g')).toBe('a\\+b\\ \\(c\\)\\ \\"d\\"\\ e\\:f\\/g');
  });
});
