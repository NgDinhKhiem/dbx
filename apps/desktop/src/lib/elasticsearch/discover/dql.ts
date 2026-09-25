/**
 * DQL (Dashboards Query Language, a.k.a. KQL) tokenizer, parser and
 * Elasticsearch/OpenSearch DSL translator.
 *
 * Semantics follow OpenSearch Dashboards' DQL as closely as practical:
 * - `field:value`           -> `match`
 * - `field:"a phrase"`      -> `match_phrase`
 * - `field:val*`            -> `query_string` scoped to the field
 * - `field:*`               -> `exists`
 * - `field >= 10`           -> `range`
 * - `field:(a or b)`        -> `bool.should` of the per-value clauses
 * - bare text               -> `multi_match` (`best_fields`, or `phrase` when quoted)
 * - `and` -> `bool.filter`, `or` -> `bool.should` + `minimum_should_match: 1`, `not` -> `bool.must_not`
 * Unquoted words are joined into one value (`foo bar` is a single `multi_match`
 * whose terms OR together, exactly like Dashboards). Adjacent expressions with
 * no operator between them (`level:ERROR service:api`) are OR-ed.
 */
import type { DiscoverField } from "./types";

export class DqlSyntaxError extends Error {
  readonly position: number;
  readonly query: string;

  constructor(message: string, position: number, query: string) {
    super(`${message} at position ${position + 1}`);
    this.name = "DqlSyntaxError";
    this.position = position;
    this.query = query;
  }
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

export type DqlTokenType = "lparen" | "rparen" | "colon" | "range" | "word" | "quoted" | "and" | "or" | "not" | "eof";

export interface DqlToken {
  type: DqlTokenType;
  /** Offset of the first character of the token in the source. */
  start: number;
  /** Offset one past the last character. */
  end: number;
  /** Unescaped text for words / quoted strings, operator text for ranges. */
  value: string;
  /** Lucene query_string form of a word (special chars escaped, unescaped `*` kept). */
  queryString?: string;
  /** Word contains an unescaped `*`. */
  wildcard?: boolean;
  /** Whitespace directly precedes the token. */
  spaceBefore: boolean;
}

const WORD_TERMINATORS = new Set(["(", ")", ":", "<", ">", '"', "{", "}"]);
const LUCENE_SPECIAL = new Set(["\\", "+", "-", "=", "&", "|", ">", "<", "!", "(", ")", "{", "}", "[", "]", "^", '"', "~", "*", "?", ":", "/"]);

function isWhitespace(ch: string): boolean {
  return ch === " " || ch === "\t" || ch === "\n" || ch === "\r" || ch === "\f" || ch === "\v" || ch === "\u00a0";
}

/** Escape one character for a Lucene `query_string` query. */
function escapeLuceneChar(ch: string): string {
  if (LUCENE_SPECIAL.has(ch) || isWhitespace(ch)) return `\\${ch}`;
  return ch;
}

/** Escape a whole string for a Lucene `query_string` query. */
export function escapeLucene(value: string): string {
  let out = "";
  for (const ch of value) out += escapeLuceneChar(ch);
  return out;
}

export function tokenizeDql(input: string): DqlToken[] {
  const tokens: DqlToken[] = [];
  let i = 0;
  let spaceBefore = false;
  while (i < input.length) {
    const ch = input[i];
    if (isWhitespace(ch)) {
      spaceBefore = true;
      i += 1;
      continue;
    }
    const start = i;
    if (ch === "(" || ch === ")") {
      tokens.push({ type: ch === "(" ? "lparen" : "rparen", start, end: i + 1, value: ch, spaceBefore });
      i += 1;
    } else if (ch === ":") {
      tokens.push({ type: "colon", start, end: i + 1, value: ch, spaceBefore });
      i += 1;
    } else if (ch === "<" || ch === ">") {
      const op = input[i + 1] === "=" ? `${ch}=` : ch;
      tokens.push({ type: "range", start, end: i + op.length, value: op, spaceBefore });
      i += op.length;
    } else if (ch === "{" || ch === "}") {
      throw new DqlSyntaxError(`Nested field queries ("${ch}") are not supported`, i, input);
    } else if (ch === '"') {
      let value = "";
      i += 1;
      let closed = false;
      while (i < input.length) {
        const c = input[i];
        if (c === "\\" && i + 1 < input.length) {
          value += input[i + 1];
          i += 2;
          continue;
        }
        if (c === '"') {
          closed = true;
          i += 1;
          break;
        }
        value += c;
        i += 1;
      }
      if (!closed) throw new DqlSyntaxError("Unterminated quoted string", start, input);
      tokens.push({ type: "quoted", start, end: i, value, queryString: `"${value.replace(/["\\]/g, "\\$&")}"`, spaceBefore });
    } else {
      let value = "";
      let queryString = "";
      let wildcard = false;
      let escaped = false;
      while (i < input.length) {
        const c = input[i];
        if (isWhitespace(c) || WORD_TERMINATORS.has(c)) break;
        if (c === "\\") {
          if (i + 1 >= input.length) throw new DqlSyntaxError("Dangling escape character", i, input);
          const next = input[i + 1];
          value += next;
          queryString += escapeLuceneChar(next);
          escaped = true;
          i += 2;
          continue;
        }
        if (c === "*") {
          wildcard = true;
          queryString += "*";
        } else {
          queryString += escapeLuceneChar(c);
        }
        value += c;
        i += 1;
      }
      const lower = value.toLowerCase();
      const keyword = !escaped && (lower === "and" || lower === "or" || lower === "not") ? (lower as "and" | "or" | "not") : null;
      if (keyword) {
        tokens.push({ type: keyword, start, end: i, value, spaceBefore });
      } else {
        tokens.push({ type: "word", start, end: i, value, queryString, wildcard, spaceBefore });
      }
    }
    spaceBefore = false;
  }
  tokens.push({ type: "eof", start: input.length, end: input.length, value: "", spaceBefore });
  return tokens;
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

export interface DqlLiteral {
  type: "literal";
  value: string;
  /** Lucene query_string representation (used for wildcards). */
  queryString: string;
  quoted: boolean;
  wildcard: boolean;
}

export type DqlValueNode = DqlLiteral | { type: "or"; children: DqlValueNode[] } | { type: "and"; children: DqlValueNode[] } | { type: "not"; child: DqlValueNode };

export type DqlRangeOperator = "gt" | "gte" | "lt" | "lte";

export type DqlNode =
  | { type: "matchAll" }
  | { type: "or"; children: DqlNode[] }
  | { type: "and"; children: DqlNode[] }
  | { type: "not"; child: DqlNode }
  | { type: "field"; field: string; value: DqlValueNode }
  | { type: "range"; field: string; operator: DqlRangeOperator; value: DqlLiteral }
  | { type: "free"; value: DqlLiteral };

const RANGE_OPERATORS: Record<string, DqlRangeOperator> = { ">": "gt", ">=": "gte", "<": "lt", "<=": "lte" };

function describeToken(token: DqlToken): string {
  if (token.type === "eof") return "end of input";
  if (token.type === "quoted") return `"${token.value}"`;
  return `"${token.value}"`;
}

class DqlParser {
  private index = 0;

  constructor(
    private readonly tokens: DqlToken[],
    private readonly input: string,
  ) {}

  private peek(offset = 0): DqlToken {
    return this.tokens[Math.min(this.index + offset, this.tokens.length - 1)];
  }

  private next(): DqlToken {
    const token = this.peek();
    if (token.type !== "eof") this.index += 1;
    return token;
  }

  private error(expected: string, token = this.peek()): never {
    throw new DqlSyntaxError(`Expected ${expected} but found ${describeToken(token)}`, token.start, this.input);
  }

  parse(): DqlNode {
    if (this.peek().type === "eof") return { type: "matchAll" };
    const node = this.parseOr();
    const token = this.peek();
    if (token.type !== "eof") {
      if (token.type === "rparen") throw new DqlSyntaxError('Unexpected ")"', token.start, this.input);
      this.error("AND, OR or end of input");
    }
    return node;
  }

  /** Tokens that can start an expression (used for implicit OR). */
  private startsExpression(token: DqlToken): boolean {
    return token.type === "word" || token.type === "quoted" || token.type === "lparen" || token.type === "not";
  }

  private parseOr(): DqlNode {
    const children = [this.parseAnd()];
    for (;;) {
      const token = this.peek();
      if (token.type === "or") {
        this.next();
        children.push(this.parseAnd());
      } else if (this.startsExpression(token)) {
        children.push(this.parseAnd());
      } else {
        break;
      }
    }
    return children.length === 1 ? children[0] : { type: "or", children };
  }

  private parseAnd(): DqlNode {
    const children = [this.parseNot()];
    while (this.peek().type === "and") {
      this.next();
      children.push(this.parseNot());
    }
    return children.length === 1 ? children[0] : { type: "and", children };
  }

  private parseNot(): DqlNode {
    if (this.peek().type === "not") {
      this.next();
      return { type: "not", child: this.parseNot() };
    }
    return this.parseSubQuery();
  }

  private parseSubQuery(): DqlNode {
    const token = this.peek();
    if (token.type === "lparen") {
      this.next();
      if (this.peek().type === "rparen") this.error("a query");
      const inner = this.parseOr();
      if (this.peek().type !== "rparen") this.error('")"');
      this.next();
      return inner;
    }
    return this.parseExpression();
  }

  private isFieldAhead(offset = 0): boolean {
    const token = this.peek(offset);
    if (token.type !== "word") return false;
    const after = this.peek(offset + 1);
    return after.type === "colon" || after.type === "range";
  }

  private parseExpression(): DqlNode {
    const token = this.peek();
    if (this.isFieldAhead()) {
      const field = this.next().value;
      const op = this.next();
      if (op.type === "range") {
        const valueToken = this.peek();
        if (valueToken.type !== "word" && valueToken.type !== "quoted") this.error("a value");
        this.next();
        return { type: "range", field, operator: RANGE_OPERATORS[op.value], value: this.literalFromToken(valueToken) };
      }
      return { type: "field", field, value: this.parseListOfValues() };
    }
    if (token.type === "word" || token.type === "quoted") {
      return { type: "free", value: this.parseValue() };
    }
    return this.error('a field, value or "("');
  }

  private parseListOfValues(): DqlValueNode {
    const token = this.peek();
    if (token.type === "lparen") {
      this.next();
      if (this.peek().type === "rparen") this.error("a value");
      const inner = this.parseOrValues();
      if (this.peek().type !== "rparen") this.error('")"');
      this.next();
      return inner;
    }
    if (token.type === "word" || token.type === "quoted") return this.parseValue();
    return this.error("a value");
  }

  private parseOrValues(): DqlValueNode {
    const children = [this.parseAndValues()];
    for (;;) {
      const token = this.peek();
      if (token.type === "or") {
        this.next();
        children.push(this.parseAndValues());
      } else if (token.type === "word" || token.type === "quoted" || token.type === "lparen" || token.type === "not") {
        children.push(this.parseAndValues());
      } else {
        break;
      }
    }
    return children.length === 1 ? children[0] : { type: "or", children };
  }

  private parseAndValues(): DqlValueNode {
    const children = [this.parseNotValues()];
    while (this.peek().type === "and") {
      this.next();
      children.push(this.parseNotValues());
    }
    return children.length === 1 ? children[0] : { type: "and", children };
  }

  private parseNotValues(): DqlValueNode {
    if (this.peek().type === "not") {
      this.next();
      return { type: "not", child: this.parseNotValues() };
    }
    return this.parseListOfValues();
  }

  private literalFromToken(token: DqlToken): DqlLiteral {
    return {
      type: "literal",
      value: token.value,
      queryString: token.queryString ?? escapeLucene(token.value),
      quoted: token.type === "quoted",
      wildcard: token.type === "word" && Boolean(token.wildcard),
    };
  }

  /**
   * A value is a quoted string, or a run of unquoted words (joined by a single
   * space) that stops before keywords, parentheses and the next `field:`.
   */
  private parseValue(): DqlLiteral {
    const first = this.next();
    if (first.type === "quoted") return this.literalFromToken(first);
    if (first.type !== "word") this.error("a value", first);
    const literal = this.literalFromToken(first);
    while (this.peek().type === "word" && !this.isFieldAhead()) {
      const word = this.next();
      literal.value += ` ${word.value}`;
      literal.queryString += `\\ ${word.queryString ?? ""}`;
      literal.wildcard = literal.wildcard || Boolean(word.wildcard);
    }
    return literal;
  }
}

export function parseDql(input: string): DqlNode {
  return new DqlParser(tokenizeDql(input), input).parse();
}

// ---------------------------------------------------------------------------
// DSL translation
// ---------------------------------------------------------------------------

export type EsQuery = Record<string, unknown>;

export interface DqlToDslOptions {
  /** Known fields, used to expand wildcard field names (`kubernetes.*:foo`). */
  fields?: readonly Pick<DiscoverField, "name" | "searchable">[];
}

function wildcardToRegExp(pattern: string): RegExp {
  const escaped = pattern.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*");
  return new RegExp(`^${escaped}$`);
}

function orQuery(children: EsQuery[]): EsQuery {
  if (children.length === 1) return children[0];
  return { bool: { should: children, minimum_should_match: 1 } };
}

function leafQuery(field: string, literal: DqlLiteral): EsQuery {
  if (!literal.quoted && literal.value === "*") return { exists: { field } };
  if (literal.wildcard) return { query_string: { fields: [field], query: literal.queryString } };
  if (literal.quoted) return { match_phrase: { [field]: literal.value } };
  return { match: { [field]: literal.value } };
}

function fieldLiteralQuery(field: string, literal: DqlLiteral, options: DqlToDslOptions): EsQuery {
  if (field === "*" && !literal.quoted && literal.value === "*") return { match_all: {} };
  if (!field.includes("*")) return leafQuery(field, literal);
  const matcher = wildcardToRegExp(field);
  const matching = (options.fields ?? []).filter((candidate) => candidate.searchable && matcher.test(candidate.name)).map((candidate) => candidate.name);
  if (matching.length > 0) return orQuery(matching.map((name) => leafQuery(name, literal)));
  // Unknown fields: let the cluster expand the field pattern.
  if (!literal.quoted && literal.value === "*") return { query_string: { query: `${field}:*` } };
  if (literal.wildcard) return { query_string: { fields: [field], query: literal.queryString } };
  return { multi_match: { query: literal.value, fields: [field], type: literal.quoted ? "phrase" : "best_fields", lenient: true } };
}

function valueToDsl(field: string, node: DqlValueNode, options: DqlToDslOptions): EsQuery {
  switch (node.type) {
    case "literal":
      return fieldLiteralQuery(field, node, options);
    case "or":
      return { bool: { should: node.children.map((child) => valueToDsl(field, child, options)), minimum_should_match: 1 } };
    case "and":
      return { bool: { filter: node.children.map((child) => valueToDsl(field, child, options)) } };
    case "not":
      return { bool: { must_not: valueToDsl(field, node.child, options) } };
  }
}

function freeTextToDsl(literal: DqlLiteral): EsQuery {
  if (!literal.quoted && literal.value === "*") return { match_all: {} };
  if (literal.wildcard) return { query_string: { query: literal.queryString } };
  return { multi_match: { type: literal.quoted ? "phrase" : "best_fields", query: literal.value, lenient: true } };
}

export function dqlAstToDsl(node: DqlNode, options: DqlToDslOptions = {}): EsQuery {
  switch (node.type) {
    case "matchAll":
      return { match_all: {} };
    case "or":
      return { bool: { should: node.children.map((child) => dqlAstToDsl(child, options)), minimum_should_match: 1 } };
    case "and":
      return { bool: { filter: node.children.map((child) => dqlAstToDsl(child, options)) } };
    case "not":
      return { bool: { must_not: dqlAstToDsl(node.child, options) } };
    case "field":
      return valueToDsl(node.field, node.value, options);
    case "range":
      return { range: { [node.field]: { [node.operator]: node.value.value } } };
    case "free":
      return freeTextToDsl(node.value);
  }
}

/** Parse DQL and translate it to a query DSL clause. Throws `DqlSyntaxError`. */
export function dqlToDsl(input: string, options: DqlToDslOptions = {}): EsQuery {
  return dqlAstToDsl(parseDql(input), options);
}

/** A caret line pointing at the error position, for monospace display under the query. */
export function formatDqlErrorPointer(error: DqlSyntaxError): string {
  return `${error.query}\n${" ".repeat(Math.max(0, error.position))}^`;
}
