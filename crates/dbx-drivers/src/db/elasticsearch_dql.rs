//! DQL (Dashboards Query Language, a.k.a. KQL) tokenizer, parser and
//! Elasticsearch/OpenSearch query DSL translator.
//!
//! Rust port of the desktop Discover view's `dql.ts`; both implementations
//! share the same test cases and must produce the same DSL:
//! - `field:value`           -> `match`
//! - `field:"a phrase"`      -> `match_phrase`
//! - `field:val*`            -> `query_string` scoped to the field
//! - `field:*`               -> `exists`
//! - `field >= 10`           -> `range`
//! - `field:(a or b)`        -> `bool.should` of the per-value clauses
//! - bare text               -> `multi_match` (`best_fields`, or `phrase` when quoted)
//! - `and` -> `bool.filter`, `or` -> `bool.should` + `minimum_should_match: 1`, `not` -> `bool.must_not`
//!
//! Unquoted words are joined into one value (`foo bar` is a single
//! `multi_match` whose terms OR together, like Dashboards). Adjacent
//! expressions with no operator between them (`level:ERROR service:api`) are
//! OR-ed. Positions are character offsets into the query.

use serde_json::{json, Value};

/// A DQL syntax error with the character offset it refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DqlSyntaxError {
    /// Full message, ending with `at position N` (1-based).
    pub message: String,
    /// 0-based character offset of the error.
    pub position: usize,
    pub query: String,
}

impl DqlSyntaxError {
    fn new(message: impl AsRef<str>, position: usize, query: &str) -> Self {
        Self {
            message: format!("{} at position {}", message.as_ref(), position + 1),
            position,
            query: query.to_string(),
        }
    }

    /// The query with a caret line under the error position, for monospace display.
    pub fn pointer(&self) -> String {
        format!("{}\n{}^", self.query, " ".repeat(self.position))
    }
}

impl std::fmt::Display for DqlSyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DqlSyntaxError {}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DqlTokenType {
    LParen,
    RParen,
    Colon,
    Range,
    Word,
    Quoted,
    And,
    Or,
    Not,
    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DqlToken {
    pub kind: DqlTokenType,
    /// Character offset of the first character of the token.
    pub start: usize,
    /// Character offset one past the last character.
    pub end: usize,
    /// Unescaped text for words / quoted strings, operator text for ranges.
    pub value: String,
    /// Lucene `query_string` form of a word or phrase.
    pub query_string: Option<String>,
    /// Word contains an unescaped `*`.
    pub wildcard: bool,
    /// Whitespace directly precedes the token.
    pub space_before: bool,
}

fn is_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{0c}' | '\u{0b}' | '\u{a0}')
}

fn is_word_terminator(ch: char) -> bool {
    matches!(ch, '(' | ')' | ':' | '<' | '>' | '"' | '{' | '}')
}

fn is_lucene_special(ch: char) -> bool {
    matches!(
        ch,
        '\\' | '+'
            | '-'
            | '='
            | '&'
            | '|'
            | '>'
            | '<'
            | '!'
            | '('
            | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | '^'
            | '"'
            | '~'
            | '*'
            | '?'
            | ':'
            | '/'
    )
}

fn push_lucene_escaped(out: &mut String, ch: char) {
    if is_lucene_special(ch) || is_whitespace(ch) {
        out.push('\\');
    }
    out.push(ch);
}

/// Escape a whole string for a Lucene `query_string` query.
pub fn escape_lucene(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        push_lucene_escaped(&mut out, ch);
    }
    out
}

pub fn tokenize_dql(input: &str) -> Result<Vec<DqlToken>, DqlSyntaxError> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut space_before = false;
    let simple = |kind, start: usize, end: usize, value: String, space_before| DqlToken {
        kind,
        start,
        end,
        value,
        query_string: None,
        wildcard: false,
        space_before,
    };
    while i < chars.len() {
        let ch = chars[i];
        if is_whitespace(ch) {
            space_before = true;
            i += 1;
            continue;
        }
        let start = i;
        match ch {
            '(' | ')' => {
                let kind = if ch == '(' { DqlTokenType::LParen } else { DqlTokenType::RParen };
                tokens.push(simple(kind, start, i + 1, ch.to_string(), space_before));
                i += 1;
            }
            ':' => {
                tokens.push(simple(DqlTokenType::Colon, start, i + 1, ":".into(), space_before));
                i += 1;
            }
            '<' | '>' => {
                let op = if chars.get(i + 1) == Some(&'=') { format!("{ch}=") } else { ch.to_string() };
                let len = op.chars().count();
                tokens.push(simple(DqlTokenType::Range, start, i + len, op, space_before));
                i += len;
            }
            '{' | '}' => {
                return Err(DqlSyntaxError::new(
                    format!("Nested field queries (\"{ch}\") are not supported"),
                    i,
                    input,
                ));
            }
            '"' => {
                let mut value = String::new();
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    let c = chars[i];
                    if c == '\\' && i + 1 < chars.len() {
                        value.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    if c == '"' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    value.push(c);
                    i += 1;
                }
                if !closed {
                    return Err(DqlSyntaxError::new("Unterminated quoted string", start, input));
                }
                let mut query_string = String::from("\"");
                for c in value.chars() {
                    if c == '"' || c == '\\' {
                        query_string.push('\\');
                    }
                    query_string.push(c);
                }
                query_string.push('"');
                tokens.push(DqlToken {
                    kind: DqlTokenType::Quoted,
                    start,
                    end: i,
                    value,
                    query_string: Some(query_string),
                    wildcard: false,
                    space_before,
                });
            }
            _ => {
                let mut value = String::new();
                let mut query_string = String::new();
                let mut wildcard = false;
                let mut escaped = false;
                while i < chars.len() {
                    let c = chars[i];
                    if is_whitespace(c) || is_word_terminator(c) {
                        break;
                    }
                    if c == '\\' {
                        let Some(&next) = chars.get(i + 1) else {
                            return Err(DqlSyntaxError::new("Dangling escape character", i, input));
                        };
                        value.push(next);
                        push_lucene_escaped(&mut query_string, next);
                        escaped = true;
                        i += 2;
                        continue;
                    }
                    if c == '*' {
                        wildcard = true;
                        query_string.push('*');
                    } else {
                        push_lucene_escaped(&mut query_string, c);
                    }
                    value.push(c);
                    i += 1;
                }
                let keyword = if escaped {
                    None
                } else {
                    match value.to_ascii_lowercase().as_str() {
                        "and" => Some(DqlTokenType::And),
                        "or" => Some(DqlTokenType::Or),
                        "not" => Some(DqlTokenType::Not),
                        _ => None,
                    }
                };
                match keyword {
                    Some(kind) => tokens.push(simple(kind, start, i, value, space_before)),
                    None => tokens.push(DqlToken {
                        kind: DqlTokenType::Word,
                        start,
                        end: i,
                        value,
                        query_string: Some(query_string),
                        wildcard,
                        space_before,
                    }),
                }
            }
        }
        space_before = false;
    }
    tokens.push(simple(DqlTokenType::Eof, chars.len(), chars.len(), String::new(), space_before));
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DqlLiteral {
    pub value: String,
    /// Lucene query_string representation (used for wildcards).
    pub query_string: String,
    pub quoted: bool,
    pub wildcard: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DqlValueNode {
    Literal(DqlLiteral),
    Or(Vec<DqlValueNode>),
    And(Vec<DqlValueNode>),
    Not(Box<DqlValueNode>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DqlRangeOperator {
    Gt,
    Gte,
    Lt,
    Lte,
}

impl DqlRangeOperator {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
        }
    }

    fn parse(op: &str) -> Option<Self> {
        match op {
            ">" => Some(Self::Gt),
            ">=" => Some(Self::Gte),
            "<" => Some(Self::Lt),
            "<=" => Some(Self::Lte),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DqlNode {
    MatchAll,
    Or(Vec<DqlNode>),
    And(Vec<DqlNode>),
    Not(Box<DqlNode>),
    Field { field: String, value: DqlValueNode },
    Range { field: String, operator: DqlRangeOperator, value: DqlLiteral },
    Free(DqlLiteral),
}

impl DqlNode {
    /// Field names referenced by the query (including wildcard names).
    pub fn field_names(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_field_names(&mut out);
        out
    }

    fn collect_field_names<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Self::Or(children) | Self::And(children) => {
                children.iter().for_each(|child| child.collect_field_names(out))
            }
            Self::Not(child) => child.collect_field_names(out),
            Self::Field { field, .. } | Self::Range { field, .. } => out.push(field),
            Self::MatchAll | Self::Free(_) => {}
        }
    }
}

fn describe_token(token: &DqlToken) -> String {
    if token.kind == DqlTokenType::Eof {
        "end of input".to_string()
    } else {
        format!("\"{}\"", token.value)
    }
}

struct DqlParser<'a> {
    tokens: Vec<DqlToken>,
    index: usize,
    input: &'a str,
}

type ParseResult<T> = Result<T, DqlSyntaxError>;

impl DqlParser<'_> {
    fn peek_at(&self, offset: usize) -> &DqlToken {
        &self.tokens[(self.index + offset).min(self.tokens.len() - 1)]
    }

    fn peek(&self) -> &DqlToken {
        self.peek_at(0)
    }

    fn kind(&self) -> DqlTokenType {
        self.peek().kind
    }

    fn next(&mut self) -> DqlToken {
        let token = self.peek().clone();
        if token.kind != DqlTokenType::Eof {
            self.index += 1;
        }
        token
    }

    fn error_at<T>(&self, expected: &str, token: &DqlToken) -> ParseResult<T> {
        Err(DqlSyntaxError::new(
            format!("Expected {expected} but found {}", describe_token(token)),
            token.start,
            self.input,
        ))
    }

    fn error<T>(&self, expected: &str) -> ParseResult<T> {
        self.error_at(expected, self.peek())
    }

    fn parse(&mut self) -> ParseResult<DqlNode> {
        if self.kind() == DqlTokenType::Eof {
            return Ok(DqlNode::MatchAll);
        }
        let node = self.parse_or()?;
        let token = self.peek();
        if token.kind != DqlTokenType::Eof {
            if token.kind == DqlTokenType::RParen {
                return Err(DqlSyntaxError::new("Unexpected \")\"", token.start, self.input));
            }
            return self.error("AND, OR or end of input");
        }
        Ok(node)
    }

    /// Tokens that can start an expression (used for implicit OR).
    fn starts_expression(kind: DqlTokenType) -> bool {
        matches!(kind, DqlTokenType::Word | DqlTokenType::Quoted | DqlTokenType::LParen | DqlTokenType::Not)
    }

    fn parse_or(&mut self) -> ParseResult<DqlNode> {
        let mut children = vec![self.parse_and()?];
        loop {
            let kind = self.kind();
            if kind == DqlTokenType::Or {
                self.next();
                children.push(self.parse_and()?);
            } else if Self::starts_expression(kind) {
                children.push(self.parse_and()?);
            } else {
                break;
            }
        }
        Ok(if children.len() == 1 { children.remove(0) } else { DqlNode::Or(children) })
    }

    fn parse_and(&mut self) -> ParseResult<DqlNode> {
        let mut children = vec![self.parse_not()?];
        while self.kind() == DqlTokenType::And {
            self.next();
            children.push(self.parse_not()?);
        }
        Ok(if children.len() == 1 { children.remove(0) } else { DqlNode::And(children) })
    }

    fn parse_not(&mut self) -> ParseResult<DqlNode> {
        if self.kind() == DqlTokenType::Not {
            self.next();
            return Ok(DqlNode::Not(Box::new(self.parse_not()?)));
        }
        self.parse_sub_query()
    }

    fn parse_sub_query(&mut self) -> ParseResult<DqlNode> {
        if self.kind() == DqlTokenType::LParen {
            self.next();
            if self.kind() == DqlTokenType::RParen {
                return self.error("a query");
            }
            let inner = self.parse_or()?;
            if self.kind() != DqlTokenType::RParen {
                return self.error("\")\"");
            }
            self.next();
            return Ok(inner);
        }
        self.parse_expression()
    }

    fn is_field_ahead(&self, offset: usize) -> bool {
        if self.peek_at(offset).kind != DqlTokenType::Word {
            return false;
        }
        matches!(self.peek_at(offset + 1).kind, DqlTokenType::Colon | DqlTokenType::Range)
    }

    fn parse_expression(&mut self) -> ParseResult<DqlNode> {
        if self.is_field_ahead(0) {
            let field = self.next().value;
            let op = self.next();
            if op.kind == DqlTokenType::Range {
                let value_token = self.peek().clone();
                if !matches!(value_token.kind, DqlTokenType::Word | DqlTokenType::Quoted) {
                    return self.error("a value");
                }
                self.next();
                let operator = DqlRangeOperator::parse(&op.value).unwrap_or(DqlRangeOperator::Gte);
                return Ok(DqlNode::Range { field, operator, value: literal_from_token(&value_token) });
            }
            return Ok(DqlNode::Field { field, value: self.parse_list_of_values()? });
        }
        if matches!(self.kind(), DqlTokenType::Word | DqlTokenType::Quoted) {
            return Ok(DqlNode::Free(self.parse_value()?));
        }
        self.error("a field, value or \"(\"")
    }

    fn parse_list_of_values(&mut self) -> ParseResult<DqlValueNode> {
        match self.kind() {
            DqlTokenType::LParen => {
                self.next();
                if self.kind() == DqlTokenType::RParen {
                    return self.error("a value");
                }
                let inner = self.parse_or_values()?;
                if self.kind() != DqlTokenType::RParen {
                    return self.error("\")\"");
                }
                self.next();
                Ok(inner)
            }
            DqlTokenType::Word | DqlTokenType::Quoted => Ok(DqlValueNode::Literal(self.parse_value()?)),
            _ => self.error("a value"),
        }
    }

    fn parse_or_values(&mut self) -> ParseResult<DqlValueNode> {
        let mut children = vec![self.parse_and_values()?];
        loop {
            let kind = self.kind();
            if kind == DqlTokenType::Or {
                self.next();
                children.push(self.parse_and_values()?);
            } else if Self::starts_expression(kind) {
                children.push(self.parse_and_values()?);
            } else {
                break;
            }
        }
        Ok(if children.len() == 1 { children.remove(0) } else { DqlValueNode::Or(children) })
    }

    fn parse_and_values(&mut self) -> ParseResult<DqlValueNode> {
        let mut children = vec![self.parse_not_values()?];
        while self.kind() == DqlTokenType::And {
            self.next();
            children.push(self.parse_not_values()?);
        }
        Ok(if children.len() == 1 { children.remove(0) } else { DqlValueNode::And(children) })
    }

    fn parse_not_values(&mut self) -> ParseResult<DqlValueNode> {
        if self.kind() == DqlTokenType::Not {
            self.next();
            return Ok(DqlValueNode::Not(Box::new(self.parse_not_values()?)));
        }
        self.parse_list_of_values()
    }

    /// A value is a quoted string, or a run of unquoted words (joined by a
    /// single space) that stops before keywords, parentheses and the next `field:`.
    fn parse_value(&mut self) -> ParseResult<DqlLiteral> {
        let first = self.next();
        if first.kind == DqlTokenType::Quoted {
            return Ok(literal_from_token(&first));
        }
        if first.kind != DqlTokenType::Word {
            return self.error_at("a value", &first);
        }
        let mut literal = literal_from_token(&first);
        while self.kind() == DqlTokenType::Word && !self.is_field_ahead(0) {
            let word = self.next();
            literal.value.push(' ');
            literal.value.push_str(&word.value);
            literal.query_string.push_str("\\ ");
            literal.query_string.push_str(word.query_string.as_deref().unwrap_or_default());
            literal.wildcard = literal.wildcard || word.wildcard;
        }
        Ok(literal)
    }
}

fn literal_from_token(token: &DqlToken) -> DqlLiteral {
    DqlLiteral {
        value: token.value.clone(),
        query_string: token.query_string.clone().unwrap_or_else(|| escape_lucene(&token.value)),
        quoted: token.kind == DqlTokenType::Quoted,
        wildcard: token.kind == DqlTokenType::Word && token.wildcard,
    }
}

pub fn parse_dql(input: &str) -> Result<DqlNode, DqlSyntaxError> {
    let tokens = tokenize_dql(input)?;
    DqlParser { tokens, index: 0, input }.parse()
}

// ---------------------------------------------------------------------------
// DSL translation
// ---------------------------------------------------------------------------

/// A known field, used to expand wildcard field names (`kubernetes.*:foo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DqlKnownField {
    pub name: String,
    pub searchable: bool,
}

/// `*`-only glob match (the only wildcard DQL field names support).
pub fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    true
}

fn or_query(mut children: Vec<Value>) -> Value {
    if children.len() == 1 {
        return children.remove(0);
    }
    json!({ "bool": { "should": children, "minimum_should_match": 1 } })
}

fn is_bare_star(literal: &DqlLiteral) -> bool {
    !literal.quoted && literal.value == "*"
}

fn leaf_query(field: &str, literal: &DqlLiteral) -> Value {
    if is_bare_star(literal) {
        return json!({ "exists": { "field": field } });
    }
    if literal.wildcard {
        return json!({ "query_string": { "fields": [field], "query": literal.query_string } });
    }
    if literal.quoted {
        return json!({ "match_phrase": { field: literal.value } });
    }
    json!({ "match": { field: literal.value } })
}

fn field_literal_query(field: &str, literal: &DqlLiteral, fields: &[DqlKnownField]) -> Value {
    if field == "*" && is_bare_star(literal) {
        return json!({ "match_all": {} });
    }
    if !field.contains('*') {
        return leaf_query(field, literal);
    }
    let matching: Vec<Value> = fields
        .iter()
        .filter(|candidate| candidate.searchable && wildcard_matches(field, &candidate.name))
        .map(|candidate| leaf_query(&candidate.name, literal))
        .collect();
    if !matching.is_empty() {
        return or_query(matching);
    }
    // Unknown fields: let the cluster expand the field pattern.
    if is_bare_star(literal) {
        return json!({ "query_string": { "query": format!("{field}:*") } });
    }
    if literal.wildcard {
        return json!({ "query_string": { "fields": [field], "query": literal.query_string } });
    }
    json!({
        "multi_match": {
            "query": literal.value,
            "fields": [field],
            "type": if literal.quoted { "phrase" } else { "best_fields" },
            "lenient": true
        }
    })
}

fn value_to_dsl(field: &str, node: &DqlValueNode, fields: &[DqlKnownField]) -> Value {
    match node {
        DqlValueNode::Literal(literal) => field_literal_query(field, literal, fields),
        DqlValueNode::Or(children) => json!({
            "bool": {
                "should": children.iter().map(|child| value_to_dsl(field, child, fields)).collect::<Vec<_>>(),
                "minimum_should_match": 1
            }
        }),
        DqlValueNode::And(children) => json!({
            "bool": { "filter": children.iter().map(|child| value_to_dsl(field, child, fields)).collect::<Vec<_>>() }
        }),
        DqlValueNode::Not(child) => json!({ "bool": { "must_not": value_to_dsl(field, child, fields) } }),
    }
}

fn free_text_to_dsl(literal: &DqlLiteral) -> Value {
    if is_bare_star(literal) {
        return json!({ "match_all": {} });
    }
    if literal.wildcard {
        return json!({ "query_string": { "query": literal.query_string } });
    }
    json!({
        "multi_match": {
            "type": if literal.quoted { "phrase" } else { "best_fields" },
            "query": literal.value,
            "lenient": true
        }
    })
}

pub fn dql_ast_to_dsl(node: &DqlNode, fields: &[DqlKnownField]) -> Value {
    match node {
        DqlNode::MatchAll => json!({ "match_all": {} }),
        DqlNode::Or(children) => json!({
            "bool": {
                "should": children.iter().map(|child| dql_ast_to_dsl(child, fields)).collect::<Vec<_>>(),
                "minimum_should_match": 1
            }
        }),
        DqlNode::And(children) => json!({
            "bool": { "filter": children.iter().map(|child| dql_ast_to_dsl(child, fields)).collect::<Vec<_>>() }
        }),
        DqlNode::Not(child) => json!({ "bool": { "must_not": dql_ast_to_dsl(child, fields) } }),
        DqlNode::Field { field, value } => value_to_dsl(field, value, fields),
        DqlNode::Range { field, operator, value } => {
            json!({ "range": { field: { operator.as_str(): value.value } } })
        }
        DqlNode::Free(literal) => free_text_to_dsl(literal),
    }
}

/// Parse DQL and translate it to a query DSL clause.
pub fn dql_to_dsl(input: &str, fields: &[DqlKnownField]) -> Result<Value, DqlSyntaxError> {
    Ok(dql_ast_to_dsl(&parse_dql(input)?, fields))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syntax_error(query: &str) -> DqlSyntaxError {
        parse_dql(query).expect_err(&format!("expected a syntax error for {query}"))
    }

    fn dsl(query: &str) -> Value {
        dql_to_dsl(query, &[]).unwrap()
    }

    fn lit(value: &str) -> DqlLiteral {
        DqlLiteral { value: value.into(), query_string: escape_lucene(value), quoted: false, wildcard: false }
    }

    fn field(name: &str, value: &str) -> DqlNode {
        DqlNode::Field { field: name.into(), value: DqlValueNode::Literal(lit(value)) }
    }

    // --- tokenizeDql ---

    #[test]
    fn splits_fields_operators_keywords_and_values_with_positions() {
        use DqlTokenType::*;
        let tokens = tokenize_dql("level:ERROR AND not (a >= 10 or msg:\"x y\")").unwrap();
        assert_eq!(
            tokens.iter().map(|token| token.kind).collect::<Vec<_>>(),
            vec![Word, Colon, Word, And, Not, LParen, Word, Range, Word, Or, Word, Colon, Quoted, RParen, Eof]
        );
        assert_eq!((tokens[0].start, tokens[0].end, tokens[0].value.as_str()), (0, 5, "level"));
        assert_eq!((tokens[7].value.as_str(), tokens[7].start), (">=", 23));
        assert_eq!(tokens[12].value, "x y");
    }

    #[test]
    fn recognizes_keywords_case_insensitively_but_not_when_escaped() {
        use DqlTokenType::*;
        let kinds: Vec<_> = tokenize_dql("a Or b AND c NoT d").unwrap().into_iter().map(|token| token.kind).collect();
        assert_eq!(kinds, vec![Word, Or, Word, And, Word, Not, Word, Eof]);
        let escaped = tokenize_dql("\\and").unwrap();
        assert_eq!((escaped[0].kind, escaped[0].value.as_str()), (Word, "and"));
    }

    #[test]
    fn unescapes_special_characters_and_tracks_wildcards() {
        let token = &tokenize_dql("url\\:http\\://x\\*y*").unwrap()[0];
        assert_eq!(token.value, "url:http://x*y*");
        assert!(token.wildcard);
        assert_eq!(token.query_string.as_deref(), Some("url\\:http\\:\\/\\/x\\*y*"));
    }

    #[test]
    fn handles_escaped_quotes_inside_phrases() {
        let token = &tokenize_dql("\"say \\\"hi\\\"\"").unwrap()[0];
        assert_eq!((token.kind, token.value.as_str()), (DqlTokenType::Quoted, "say \"hi\""));
    }

    // --- parseDql ---

    #[test]
    fn returns_match_all_for_empty_input() {
        assert_eq!(parse_dql("   ").unwrap(), DqlNode::MatchAll);
    }

    #[test]
    fn gives_and_precedence_over_or() {
        assert_eq!(
            parse_dql("a:1 or b:2 and c:3").unwrap(),
            DqlNode::Or(vec![field("a", "1"), DqlNode::And(vec![field("b", "2"), field("c", "3")])])
        );
    }

    #[test]
    fn respects_parentheses() {
        assert_eq!(
            parse_dql("(a:1 or b:2) and c:3").unwrap(),
            DqlNode::And(vec![DqlNode::Or(vec![field("a", "1"), field("b", "2")]), field("c", "3")])
        );
    }

    #[test]
    fn binds_not_tighter_than_and() {
        assert_eq!(
            parse_dql("not a:1 and b:2").unwrap(),
            DqlNode::And(vec![DqlNode::Not(Box::new(field("a", "1"))), field("b", "2")])
        );
    }

    #[test]
    fn joins_adjacent_unquoted_words_into_one_value() {
        assert_eq!(
            parse_dql("hello big world").unwrap(),
            DqlNode::Free(DqlLiteral {
                value: "hello big world".into(),
                query_string: "hello\\ big\\ world".into(),
                quoted: false,
                wildcard: false
            })
        );
    }

    #[test]
    fn stops_a_value_before_the_next_field_expression_and_ors_adjacent_expressions() {
        assert_eq!(
            parse_dql("level:WARN service.name:api").unwrap(),
            DqlNode::Or(vec![field("level", "WARN"), field("service.name", "api")])
        );
    }

    #[test]
    fn parses_value_lists() {
        let value = |v: &str| DqlValueNode::Literal(lit(v));
        assert_eq!(
            parse_dql("level:(ERROR or WARN and not DEBUG)").unwrap(),
            DqlNode::Field {
                field: "level".into(),
                value: DqlValueNode::Or(vec![
                    value("ERROR"),
                    DqlValueNode::And(vec![value("WARN"), DqlValueNode::Not(Box::new(value("DEBUG")))])
                ])
            }
        );
    }

    #[test]
    fn parses_range_operators() {
        let DqlNode::Range { field, operator, value } = parse_dql("severity >= 40").unwrap() else { panic!() };
        assert_eq!((field.as_str(), operator, value.value.as_str()), ("severity", DqlRangeOperator::Gte, "40"));
        assert!(matches!(parse_dql("severity<40").unwrap(), DqlNode::Range { operator: DqlRangeOperator::Lt, .. }));
        assert!(matches!(parse_dql("severity > 40").unwrap(), DqlNode::Range { operator: DqlRangeOperator::Gt, .. }));
        let DqlNode::Range { operator, value, .. } = parse_dql("@timestamp <= \"2026-09-23T10:00:00\"").unwrap() else {
            panic!()
        };
        assert_eq!(operator, DqlRangeOperator::Lte);
        assert_eq!((value.value.as_str(), value.quoted), ("2026-09-23T10:00:00", true));
    }

    #[test]
    fn reports_unterminated_strings_with_their_position() {
        let error = syntax_error("message:\"abc");
        assert_eq!(error.position, 8);
        assert_eq!(error.message, "Unterminated quoted string at position 9");
        assert_eq!(error.pointer(), "message:\"abc\n        ^");
    }

    #[test]
    fn reports_a_missing_closing_parenthesis() {
        let error = syntax_error("(a:1 or b:2");
        assert!(error.message.contains("Expected \")\" but found end of input"), "{}", error.message);
        assert_eq!(error.position, 11);
    }

    #[test]
    fn reports_a_missing_value() {
        assert!(syntax_error("level:").message.contains("Expected a value but found end of input"));
        assert!(syntax_error("level: and x").message.contains("Expected a value but found \"and\""));
        assert!(syntax_error("severity >=").message.contains("Expected a value"));
    }

    #[test]
    fn reports_dangling_operators_and_stray_parentheses() {
        assert!(syntax_error("a:1 and").message.contains("Expected a field, value or"));
        assert!(syntax_error("a:1)").message.contains("Unexpected \")\""));
        assert!(syntax_error("()").message.contains("Expected a query"));
        assert!(syntax_error("a\\").message.contains("Dangling escape"));
    }

    #[test]
    fn rejects_nested_field_syntax_clearly() {
        assert!(syntax_error("items:{ name:x }").message.contains("Nested field queries"));
    }

    #[test]
    fn positions_count_characters_not_bytes() {
        let error = syntax_error("msg:\"héllo\" and");
        assert_eq!(error.position, 15);
    }

    // --- dqlToDsl ---

    #[test]
    fn translates_field_value_to_match() {
        assert_eq!(dsl("level:ERROR"), json!({ "match": { "level": "ERROR" } }));
    }

    #[test]
    fn translates_quoted_values_to_match_phrase() {
        assert_eq!(
            dsl("message:\"connection refused\""),
            json!({ "match_phrase": { "message": "connection refused" } })
        );
    }

    #[test]
    fn translates_wildcards_to_a_field_scoped_query_string() {
        assert_eq!(
            dsl("service.name:aml-*"),
            json!({ "query_string": { "fields": ["service.name"], "query": "aml\\-*" } })
        );
    }

    #[test]
    fn translates_field_star_to_exists() {
        assert_eq!(dsl("trace_id:*"), json!({ "exists": { "field": "trace_id" } }));
        assert_eq!(dsl("trace_id:\"*\""), json!({ "match_phrase": { "trace_id": "*" } }));
    }

    #[test]
    fn translates_and_or_not_to_bool_clauses() {
        assert_eq!(
            dsl("level:ERROR and not service:api or is_error:true"),
            json!({
                "bool": {
                    "should": [
                        { "bool": { "filter": [
                            { "match": { "level": "ERROR" } },
                            { "bool": { "must_not": { "match": { "service": "api" } } } }
                        ] } },
                        { "match": { "is_error": "true" } }
                    ],
                    "minimum_should_match": 1
                }
            })
        );
    }

    #[test]
    fn translates_value_lists() {
        assert_eq!(
            dsl("level:(ERROR or WARN)"),
            json!({ "bool": { "should": [{ "match": { "level": "ERROR" } }, { "match": { "level": "WARN" } }], "minimum_should_match": 1 } })
        );
        assert_eq!(
            dsl("message:(\"a b\" and c)"),
            json!({ "bool": { "filter": [{ "match_phrase": { "message": "a b" } }, { "match": { "message": "c" } }] } })
        );
    }

    #[test]
    fn translates_ranges() {
        assert_eq!(
            dsl("severity >= 40 and http_status < 500"),
            json!({ "bool": { "filter": [{ "range": { "severity": { "gte": "40" } } }, { "range": { "http_status": { "lt": "500" } } }] } })
        );
    }

    #[test]
    fn translates_free_text_to_multi_match_or_query_string() {
        assert_eq!(
            dsl("timeout error"),
            json!({ "multi_match": { "type": "best_fields", "query": "timeout error", "lenient": true } })
        );
        assert_eq!(
            dsl("\"bean creation\""),
            json!({ "multi_match": { "type": "phrase", "query": "bean creation", "lenient": true } })
        );
        assert_eq!(dsl("Bean*"), json!({ "query_string": { "query": "Bean*" } }));
        assert_eq!(dsl("*"), json!({ "match_all": {} }));
        assert_eq!(dsl(""), json!({ "match_all": {} }));
    }

    #[test]
    fn keeps_escaped_characters_literal() {
        assert_eq!(dsl("path:C\\:\\\\tmp"), json!({ "match": { "path": "C:\\tmp" } }));
        assert_eq!(dsl("name:a\\*b"), json!({ "match": { "name": "a*b" } }));
        assert_eq!(dsl("level:\\or"), json!({ "match": { "level": "or" } }));
    }

    #[test]
    fn expands_wildcard_field_names_against_known_fields() {
        let fields = ["kubernetes.pod", "kubernetes.ns", "level"]
            .map(|name| DqlKnownField { name: name.into(), searchable: true })
            .to_vec();
        assert_eq!(
            dql_to_dsl("kubernetes.*:web", &fields).unwrap(),
            json!({ "bool": { "should": [{ "match": { "kubernetes.pod": "web" } }, { "match": { "kubernetes.ns": "web" } }], "minimum_should_match": 1 } })
        );
        assert_eq!(
            dql_to_dsl("unknown.*:web", &fields).unwrap(),
            json!({ "multi_match": { "query": "web", "fields": ["unknown.*"], "type": "best_fields", "lenient": true } })
        );
        assert_eq!(dsl("*:*"), json!({ "match_all": {} }));
    }

    #[test]
    fn skips_non_searchable_fields_when_expanding() {
        let fields = vec![
            DqlKnownField { name: "k8s.pod".into(), searchable: true },
            DqlKnownField { name: "k8s.raw".into(), searchable: false },
        ];
        assert_eq!(dql_to_dsl("k8s.*:web", &fields).unwrap(), json!({ "match": { "k8s.pod": "web" } }));
    }

    #[test]
    fn combines_nested_parentheses_and_negated_groups() {
        assert_eq!(
            dsl("not (level:ERROR or level:WARN) and service:(api or web)"),
            json!({
                "bool": {
                    "filter": [
                        { "bool": { "must_not": { "bool": { "should": [{ "match": { "level": "ERROR" } }, { "match": { "level": "WARN" } }], "minimum_should_match": 1 } } } },
                        { "bool": { "should": [{ "match": { "service": "api" } }, { "match": { "service": "web" } }], "minimum_should_match": 1 } }
                    ]
                }
            })
        );
    }

    #[test]
    fn escapes_reserved_characters_and_whitespace() {
        assert_eq!(escape_lucene("a+b (c) \"d\" e:f/g"), "a\\+b\\ \\(c\\)\\ \\\"d\\\"\\ e\\:f\\/g");
    }

    #[test]
    fn wildcard_matching_and_field_names() {
        assert!(wildcard_matches("kubernetes.*", "kubernetes.pod.name"));
        assert!(wildcard_matches("*.name", "pod.name"));
        assert!(wildcard_matches("a*b*c", "aXbYc"));
        assert!(!wildcard_matches("a*b*c", "aXc"));
        assert!(!wildcard_matches("level", "levels"));
        assert_eq!(parse_dql("a:1 and not (b >= 2 or c.*:x)").unwrap().field_names(), vec!["a", "b", "c.*"]);
    }
}
