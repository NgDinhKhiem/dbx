use crate::models::connection::DatabaseType;

pub fn format_pg_array_sql_literal(arr: &[serde_json::Value]) -> String {
    if arr.is_empty() {
        return "'{}'".to_string();
    }
    let elements: Vec<String> = arr.iter().map(format_pg_array_element).collect();
    let inner = format!("{{{}}}", elements.join(","));
    format!("'{}'", inner.replace('\\', "\\\\").replace('\'', "''"))
}

pub fn format_pg_array_element(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                return "{}".to_string();
            }
            let elements: Vec<String> = arr.iter().map(format_pg_array_element).collect();
            format!("{{{}}}", elements.join(","))
        }
        serde_json::Value::String(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{}\"", escaped)
        }
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        serde_json::Value::Object(o) => {
            let json = serde_json::to_string(o).unwrap_or_default();
            let escaped = json.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{}\"", escaped)
        }
    }
}

pub fn format_ch_array_sql_literal(arr: &[serde_json::Value]) -> String {
    if arr.is_empty() {
        return "[]".to_string();
    }
    let elements: Vec<String> = arr.iter().map(format_ch_array_element).collect();
    format!("[{}]", elements.join(","))
}

pub fn format_ch_array_element(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                return "[]".to_string();
            }
            let elements: Vec<String> = arr.iter().map(format_ch_array_element).collect();
            format!("[{}]", elements.join(","))
        }
        serde_json::Value::String(s) => {
            let escaped = s.replace('\\', "\\\\").replace('\'', "''");
            format!("'{}'", escaped)
        }
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        serde_json::Value::Object(o) => {
            let json = serde_json::to_string(o).unwrap_or_default();
            format!("'{}'", json.replace('\\', "\\\\").replace('\'', "''"))
        }
    }
}

pub fn quote_postgres_string_literal(value: &str) -> String {
    if !value.contains('\\') && !value.chars().any(|character| character.is_ascii_control()) {
        return quote_string_literal(value);
    }

    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\x08' => escaped.push_str("\\b"),
            '\x0c' => escaped.push_str("\\f"),
            '\'' => escaped.push_str("''"),
            character if character.is_ascii_control() => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                let byte = character as u8;
                escaped.push_str("\\x");
                escaped.push(HEX[(byte >> 4) as usize] as char);
                escaped.push(HEX[(byte & 0x0F) as usize] as char);
            }
            character => escaped.push(character),
        }
    }

    // Escape string constants keep control characters out of the physical
    // script and remain correct regardless of standard_conforming_strings.
    format!("E'{escaped}'")
}

pub fn is_postgres_vector_type(column_type: Option<&str>) -> bool {
    column_type
        .map(|column_type| {
            let normalized = column_type.trim().trim_matches('"').to_ascii_lowercase();
            if normalized.trim_end().ends_with("[]") {
                return false;
            }
            let base = normalized.split(['(', ' ', '\t', '\n']).next().unwrap_or("").trim_matches('"');
            matches!(base, "vector" | "halfvec") || base.ends_with(".vector") || base.ends_with(".halfvec")
        })
        .unwrap_or(false)
}

pub fn format_postgres_vector_sql_literal(value: &serde_json::Value) -> String {
    if value.is_null() {
        return "NULL".to_string();
    }
    let text = match value {
        // pgvector vector/halfvec are scalar extension types whose importable
        // literal grammar uses square brackets, unlike PostgreSQL arrays.
        serde_json::Value::Array(arr) => {
            let elements = arr.iter().map(format_postgres_vector_element).collect::<Vec<_>>();
            format!("[{}]", elements.join(","))
        }
        serde_json::Value::String(text) => text.to_string(),
        _ => value.to_string(),
    };
    quote_postgres_string_literal(&text)
}

pub fn format_postgres_vector_element(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.trim().to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null => "NULL".to_string(),
        _ => value.to_string(),
    }
}

pub fn quote_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Quote a string literal for engines whose ordinary string literals treat a backslash as an
/// escape character (the MySQL family, ClickHouse, ...).
///
/// Backslashes are doubled *and* single quotes are doubled (`''`, never `\'`). The result stays
/// a single closed literal whether or not MySQL's `NO_BACKSLASH_ESCAPES` SQL mode is active:
/// with backslash escapes `\\` is one backslash and `''` one quote; without them `\\` is two
/// backslashes and `''` is still one quote. Doubling only the quote would let a value ending in
/// `\` (e.g. `x\'; DROP TABLE t; -- `) escape the closing delimiter, and `\'` would terminate
/// the literal early under `NO_BACKSLASH_ESCAPES`.
pub fn quote_backslash_escaped_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

/// Engines whose ordinary `'...'` literals interpret backslash escapes (at least in their default
/// configuration). Engines that may or may not do so depending on a profile are included as
/// well, because [`quote_backslash_escaped_string_literal`] is injection-safe either way.
pub fn database_uses_backslash_string_escapes(database_type: DatabaseType) -> bool {
    matches!(
        database_type,
        DatabaseType::Mysql
            | DatabaseType::Doris
            | DatabaseType::StarRocks
            | DatabaseType::Goldendb
            | DatabaseType::Sundb
            | DatabaseType::Databend
            | DatabaseType::Gbase
            | DatabaseType::ClickHouse
            | DatabaseType::ManticoreSearch
    )
}

/// Quote a string literal for the given target engine. MySQL-family engines and ClickHouse use
/// [`quote_backslash_escaped_string_literal`], Manticore uses `\\` and `\'`, and everything else
/// doubles single quotes only.
pub fn quote_string_literal_for_database(database_type: Option<DatabaseType>, value: &str) -> String {
    match database_type {
        // Manticore's SphinxQL lexer only understands backslash escapes, not `''`.
        Some(DatabaseType::ManticoreSearch) => format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'")),
        Some(database_type) if database_uses_backslash_string_escapes(database_type) => {
            quote_backslash_escaped_string_literal(value)
        }
        _ => quote_string_literal(value),
    }
}

/// Minimal MySQL literal scanner used to prove the quoted output is exactly one token.
#[cfg(test)]
pub(crate) fn mysql_literal_is_single_token(sql: &str, backslash_escapes: bool) -> bool {
    let Some(inner) = sql.strip_prefix('\'').and_then(|value| value.strip_suffix('\'')) else {
        return false;
    };
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if backslash_escapes => {
                if chars.next().is_none() {
                    return false;
                }
            }
            '\'' => {
                if chars.next() != Some('\'') {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backslash_escaped_literal_neutralizes_trailing_backslash_breakout() {
        let payload = "x\\'; DROP TABLE users; -- ";
        let quoted = quote_backslash_escaped_string_literal(payload);
        assert_eq!(quoted, "'x\\\\''; DROP TABLE users; -- '");
        // Every quote from the value is doubled and every backslash is paired, so the only
        // unpaired quotes are the delimiters, both with and without backslash escapes.
        assert!(mysql_literal_is_single_token(&quoted, true));
        assert!(mysql_literal_is_single_token(&quoted, false));
        // Quote doubling alone lets the payload break out under backslash escapes.
        assert!(!mysql_literal_is_single_token(&quote_string_literal(payload), true));
    }

    #[test]
    fn quote_string_literal_for_database_is_dialect_aware() {
        assert_eq!(quote_string_literal_for_database(Some(DatabaseType::Mysql), "a\\b'c"), "'a\\\\b''c'");
        assert_eq!(quote_string_literal_for_database(Some(DatabaseType::ClickHouse), "a\\b"), "'a\\\\b'");
        assert_eq!(quote_string_literal_for_database(Some(DatabaseType::Postgres), "a\\b'c"), "'a\\b''c'");
        assert_eq!(quote_string_literal_for_database(Some(DatabaseType::Oracle), "a\\b'c"), "'a\\b''c'");
        assert_eq!(quote_string_literal_for_database(None, "it's"), "'it''s'");
        assert_eq!(
            quote_string_literal_for_database(Some(DatabaseType::ManticoreSearch), "a\\b'c"),
            "'a\\\\b\\'c'"
        );
    }
}
