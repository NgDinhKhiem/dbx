//! String-literal handling for DDL emitted by schema diff.
//!
//! Comments and defaults are read from the *source* database and replayed as DDL on the *target*
//! database, so every literal has to be re-encoded for the target's lexer. A MySQL-family target
//! treats backslash as an escape character, so a source value ending in `\` would otherwise escape
//! the closing quote and let the rest of the value run as SQL.

use crate::models::connection::DatabaseType;
use crate::value_literals::{database_uses_backslash_string_escapes, quote_string_literal_for_database};

/// Quote an arbitrary value as a string literal for the target database.
pub(super) fn target_string_literal(value: &str, target: DatabaseType) -> String {
    quote_string_literal_for_database(Some(target), value)
}

/// Decode one complete MySQL `'...'` literal as the MySQL-family source reported it (backslash
/// escapes enabled, `''` also accepted). Returns `None` unless `literal` is exactly one literal.
pub(super) fn decode_mysql_string_literal(literal: &str) -> Option<String> {
    let inner = literal.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut decoded = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                let escaped = chars.next()?;
                match escaped {
                    '0' => decoded.push('\0'),
                    'b' => decoded.push('\u{8}'),
                    'n' => decoded.push('\n'),
                    'r' => decoded.push('\r'),
                    't' => decoded.push('\t'),
                    'Z' => decoded.push('\u{1a}'),
                    // MySQL keeps the backslash for these so LIKE patterns survive.
                    '%' | '_' => {
                        decoded.push('\\');
                        decoded.push(escaped);
                    }
                    other => decoded.push(other),
                }
            }
            '\'' => {
                if chars.next()? != '\'' {
                    return None;
                }
                decoded.push('\'');
            }
            other => decoded.push(other),
        }
    }
    Some(decoded)
}

/// Re-encode a MySQL-family quoted default (`'x'`, `N'x'`, `b'1'`, `x'1f'`, ...) for the target.
///
/// A value that is not exactly one well-formed literal is quoted as a plain string instead of
/// being passed through, so a crafted default can never close the literal early.
pub(super) fn reencode_mysql_quoted_default(value: &str, target: DatabaseType) -> String {
    let (prefix, literal) = match value.find('\'') {
        Some(index) => value.split_at(index),
        None => return target_string_literal(value, target),
    };
    let Some(decoded) = decode_mysql_string_literal(literal) else {
        return target_string_literal(value, target);
    };
    match prefix.to_ascii_lowercase().as_str() {
        "" | "n" | "e" => format!("{prefix}{}", target_string_literal(&decoded, target)),
        "b" if !decoded.is_empty() && decoded.chars().all(|ch| ch == '0' || ch == '1') => {
            format!("{prefix}'{decoded}'")
        }
        "x" if decoded.chars().all(|ch| ch.is_ascii_hexdigit()) => format!("{prefix}'{decoded}'"),
        _ => target_string_literal(value, target),
    }
}

/// Whether `value` is one parenthesised expression, `( ... )`, that can be replayed verbatim:
/// the opening parenthesis closes on the last character, there is no statement separator or
/// comment marker outside quotes, and quoted sections contain no backslash (so they lex the same
/// with and without backslash escapes).
pub(super) fn is_self_contained_parenthesized_expression(value: &str) -> bool {
    if !value.starts_with('(') || !value.ends_with(')') {
        return false;
    }
    let chars: Vec<char> = value.chars().collect();
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < chars.len() {
        match chars[index] {
            quote @ ('\'' | '"' | '`') => {
                index += 1;
                loop {
                    let Some(&ch) = chars.get(index) else {
                        return false;
                    };
                    if ch == '\\' {
                        return false;
                    }
                    if ch == quote {
                        if chars.get(index + 1) == Some(&quote) {
                            index += 2;
                            continue;
                        }
                        break;
                    }
                    index += 1;
                }
            }
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
                if depth == 0 && index + 1 != chars.len() {
                    return false;
                }
            }
            ';' | '#' => return false,
            '-' if chars.get(index + 1) == Some(&'-') => return false,
            '/' if chars.get(index + 1) == Some(&'*') => return false,
            _ => {}
        }
        index += 1;
    }
    depth == 0
}

/// Re-encode the standard (`''`-escaped, backslash-literal) string literals inside an expression
/// that came from a non-MySQL source so it lexes identically on a backslash-escaping target.
///
/// Targets that keep backslashes literal get the expression unchanged. Returns `None` when the
/// expression cannot be tokenised safely; callers must then quote the whole value instead.
pub(super) fn retarget_standard_expression_literals(expression: &str, target: DatabaseType) -> Option<String> {
    if !database_uses_backslash_string_escapes(target) || !expression.contains('\\') {
        return Some(expression.to_string());
    }
    let mut output = String::with_capacity(expression.len() + 8);
    let mut chars = expression.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                let mut content = String::new();
                loop {
                    match chars.next()? {
                        '\'' if chars.peek() == Some(&'\'') => {
                            chars.next();
                            content.push('\'');
                        }
                        '\'' => break,
                        other => content.push(other),
                    }
                }
                output.push_str(&target_string_literal(&content, target));
            }
            // A double-quoted identifier is a string with backslash escapes on MySQL without
            // ANSI_QUOTES; one containing a backslash cannot be carried over safely.
            '"' | '`' => {
                output.push(ch);
                loop {
                    let next = chars.next()?;
                    if next == '\\' {
                        return None;
                    }
                    output.push(next);
                    if next == ch {
                        if chars.peek() == Some(&ch) {
                            output.push(chars.next()?);
                            continue;
                        }
                        break;
                    }
                }
            }
            other => output.push(other),
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value_literals::mysql_literal_is_single_token;

    #[test]
    fn decodes_mysql_literals() {
        assert_eq!(decode_mysql_string_literal("'it''s'").as_deref(), Some("it's"));
        assert_eq!(decode_mysql_string_literal("'it\\'s'").as_deref(), Some("it's"));
        assert_eq!(decode_mysql_string_literal("'a\\\\b'").as_deref(), Some("a\\b"));
        assert_eq!(decode_mysql_string_literal("'a\\nb'").as_deref(), Some("a\nb"));
        assert_eq!(decode_mysql_string_literal("'\\''; DROP TABLE t; -- '"), None);
        assert_eq!(decode_mysql_string_literal("'x' OR '1'"), None);
    }

    #[test]
    fn reencodes_quoted_defaults_for_the_target() {
        assert_eq!(reencode_mysql_quoted_default("'guest'", DatabaseType::Mysql), "'guest'");
        assert_eq!(reencode_mysql_quoted_default("'a\\\\b'", DatabaseType::Mysql), "'a\\\\b'");
        assert_eq!(reencode_mysql_quoted_default("'a\\\\b'", DatabaseType::Postgres), "'a\\b'");
        assert_eq!(reencode_mysql_quoted_default("N'x'", DatabaseType::SqlServer), "N'x'");
        assert_eq!(reencode_mysql_quoted_default("b'101'", DatabaseType::Mysql), "b'101'");
        assert_eq!(reencode_mysql_quoted_default("x'DEAD'", DatabaseType::Mysql), "x'DEAD'");
        // A crafted "literal" that only looks closed is quoted as a whole instead.
        let crafted = "'\\''; DROP TABLE t; -- '";
        let encoded = reencode_mysql_quoted_default(crafted, DatabaseType::Mysql);
        assert!(mysql_literal_is_single_token(&encoded, true));
        assert!(mysql_literal_is_single_token(&encoded, false));
        let crafted_hex = "x''; DROP TABLE t; -- '";
        let encoded = reencode_mysql_quoted_default(crafted_hex, DatabaseType::Mysql);
        assert!(mysql_literal_is_single_token(&encoded, true));
    }

    #[test]
    fn parenthesized_expression_must_be_self_contained() {
        assert!(is_self_contained_parenthesized_expression("(uuid())"));
        assert!(is_self_contained_parenthesized_expression("(concat('a', 'b'))"));
        assert!(!is_self_contained_parenthesized_expression("(1); DROP TABLE t; SELECT (1)"));
        assert!(!is_self_contained_parenthesized_expression("(1) -- )"));
        assert!(!is_self_contained_parenthesized_expression("('x\\') ; DROP TABLE t; (')"));
        assert!(!is_self_contained_parenthesized_expression("(1)/*)*/"));
    }

    #[test]
    fn retargets_standard_literals_for_backslash_targets() {
        // A Postgres default `'\' || '; DROP TABLE t; -- '` is harmless on Postgres but would
        // break out of the first literal on MySQL if replayed verbatim.
        let expression = "('\\'::text || '; DROP TABLE t; -- '::text)";
        let retargeted = retarget_standard_expression_literals(expression, DatabaseType::Mysql).unwrap();
        assert_eq!(retargeted, "('\\\\'::text || '; DROP TABLE t; -- '::text)");
        assert_eq!(
            retarget_standard_expression_literals(expression, DatabaseType::Postgres).as_deref(),
            Some(expression)
        );
        assert_eq!(retarget_standard_expression_literals("'unterminated\\", DatabaseType::Mysql), None);
        assert_eq!(retarget_standard_expression_literals("\"a\\\" || 'x'", DatabaseType::Mysql), None);
    }
}
