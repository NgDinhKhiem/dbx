use rusqlite::{Connection, DatabaseName, OpenFlags};
use serde_json::json;
use sqlparser::ast::Statement;
use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use crate::protocol::{WorkerBody, WorkerOp, WorkerRequest, WorkerResponse};

const MAX_BLOB_BYTES: usize = 512 * 1024;
/// TEXT cells are cut at this many UTF-8 bytes (on a char boundary). Like an
/// oversized BLOB, a cut value is reported through the response `truncated` flag.
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_JSON_BYTES: usize = 8 * 1024 * 1024;

pub fn run_stdio() -> Result<(), String> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(stdin.lock(), stdout.lock())
}

/// Serves JSONL requests until EOF. A line that is not a valid request (bad
/// JSON, unknown op, invalid UTF-8) is answered with an error response instead
/// of terminating the worker; its id is recovered when possible, otherwise 0.
fn serve<R: BufRead, W: Write>(mut input: R, mut output: W) -> Result<(), String> {
    let mut connection = None;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if input.read_until(b'\n', &mut buffer).map_err(|e| e.to_string())? == 0 {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&buffer);
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<WorkerRequest>(&line) {
            Ok(request) => handle(&mut connection, request),
            Err(error) => WorkerResponse {
                id: recover_request_id(&line),
                body: WorkerBody::err(format!("invalid worker request: {error}")),
            },
        };
        serde_json::to_writer(&mut output, &response).map_err(|e| e.to_string())?;
        output.write_all(b"\n").map_err(|e| e.to_string())?;
        output.flush().map_err(|e| e.to_string())?;
    }
}

fn recover_request_id(line: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| value.get("id").and_then(serde_json::Value::as_u64))
        .unwrap_or(0)
}

fn handle(connection: &mut Option<Connection>, request: WorkerRequest) -> WorkerResponse {
    let body = match request.op {
        WorkerOp::Open { path } => match open_database(&path) {
            Ok(opened) => {
                *connection = Some(opened);
                WorkerBody::ok()
            }
            Err(error) => WorkerBody::err(error),
        },
        WorkerOp::Query { sql, max_rows } => match connection.as_mut() {
            Some(conn) => query(conn, &sql, max_rows.unwrap_or(10_000)),
            None => WorkerBody::err("SQLite worker has no open database"),
        },
        WorkerOp::Backup { dest } => match connection.as_mut() {
            Some(conn) => backup(conn, &dest),
            None => WorkerBody::err("SQLite worker has no open database"),
        },
        WorkerOp::Restore { src } => match connection.as_mut() {
            Some(conn) => restore(conn, &src),
            None => WorkerBody::err("SQLite worker has no open database"),
        },
        WorkerOp::Ping => WorkerBody::pong(),
        WorkerOp::Close => {
            *connection = None;
            WorkerBody::ok()
        }
    };
    WorkerResponse { id: request.id, body }
}

fn open_database(path: &str) -> Result<Connection, String> {
    if path.trim().is_empty() {
        return Err("SQLite path is empty".to_string());
    }
    if path.contains('\0') {
        return Err("SQLite path contains NUL".to_string());
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI;
    if !Path::new(path).is_file() {
        return Err(format!("File does not exist: {path}"));
    }
    let conn = Connection::open_with_flags(path, flags).map_err(|e| format!("failed to open SQLite file: {e}"))?;
    conn.busy_timeout(Duration::from_secs(10)).map_err(|e| e.to_string())?;
    Ok(conn)
}

fn query(conn: &Connection, sql: &str, max_rows: usize) -> WorkerBody {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return WorkerBody::err("SQL is empty");
    }
    if sqlite_statement_returns_rows(trimmed) {
        query_statement(conn, trimmed, max_rows)
    } else {
        match conn.execute_batch(trimmed) {
            Ok(()) => WorkerBody::query(Vec::new(), Vec::new(), Vec::new(), conn.changes(), false),
            Err(error) => WorkerBody::err(error.to_string()),
        }
    }
}

fn sqlite_statement_returns_rows(sql: &str) -> bool {
    if sqlite_starts_with_keyword(sql, &["SELECT", "PRAGMA", "EXPLAIN", "WITH"]) {
        return true;
    }
    let Ok(statements) = Parser::parse_sql(&SQLiteDialect {}, sql) else {
        return false;
    };
    let [statement] = statements.as_slice() else {
        return false;
    };
    match statement {
        Statement::Insert(insert) => insert.returning.is_some(),
        Statement::Update(update) => update.returning.is_some(),
        Statement::Delete(delete) => delete.returning.is_some(),
        _ => false,
    }
}

fn sqlite_starts_with_keyword(sql: &str, keywords: &[&str]) -> bool {
    let rest = skip_sqlite_trivia(sql);
    keywords.iter().any(|keyword| {
        rest.len() >= keyword.len()
            && rest[..keyword.len()].eq_ignore_ascii_case(keyword)
            && rest.as_bytes().get(keyword.len()).is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
    })
}

fn skip_sqlite_trivia(sql: &str) -> &str {
    let mut rest = sql.trim_start();
    loop {
        if rest.starts_with("--") {
            rest = rest.split_once('\n').map(|(_, tail)| tail).unwrap_or("").trim_start();
            continue;
        }
        if let Some(rest_of_block) = rest.strip_prefix("/*") {
            rest = rest_of_block.split_once("*/").map(|(_, tail)| tail).unwrap_or("").trim_start();
            continue;
        }
        break;
    }
    rest
}

fn query_statement(conn: &Connection, sql: &str, max_rows: usize) -> WorkerBody {
    match conn.prepare(sql) {
        Ok(mut stmt) => {
            let column_count = stmt.column_count();
            if column_count == 0 {
                return match stmt.execute([]) {
                    Ok(changed) => WorkerBody::query(Vec::new(), Vec::new(), Vec::new(), changed as u64, false),
                    Err(error) => WorkerBody::err(error.to_string()),
                };
            }
            let columns = stmt.column_names().iter().map(|name| (*name).to_string()).collect::<Vec<_>>();
            let column_decl_types =
                stmt.columns().iter().map(|column| column.decl_type().map(str::to_string)).collect::<Vec<_>>();
            let column_types =
                column_decl_types.iter().map(|decl| decl.clone().unwrap_or_default()).collect::<Vec<_>>();
            let mut rows = Vec::new();
            let mut truncated = false;
            let mut encoded_bytes = 0usize;
            match stmt.query([]) {
                Ok(mut mapped) => loop {
                    let row = match mapped.next() {
                        Ok(Some(row)) => row,
                        Ok(None) => break,
                        Err(error) => return WorkerBody::err(error.to_string()),
                    };
                    if rows.len() >= max_rows {
                        truncated = true;
                        break;
                    }
                    let mut values = Vec::with_capacity(columns.len());
                    for index in 0..columns.len() {
                        match row.get_ref(index) {
                            Ok(value) => {
                                let (json, value_truncated) =
                                    value_to_json(value, column_decl_types.get(index).and_then(Option::as_deref));
                                truncated |= value_truncated;
                                values.push(json);
                            }
                            Err(error) => return WorkerBody::err(error.to_string()),
                        }
                    }
                    let row_size = estimated_row_json_bytes(&values);
                    if encoded_bytes + row_size > MAX_RESPONSE_JSON_BYTES {
                        if rows.is_empty() {
                            return WorkerBody::err(format!(
                                "SQLite worker result row is larger than the {} MiB response limit; select fewer or smaller columns",
                                MAX_RESPONSE_JSON_BYTES / (1024 * 1024)
                            ));
                        }
                        truncated = true;
                        break;
                    }
                    encoded_bytes += row_size;
                    rows.push(values);
                },
                Err(error) => return WorkerBody::err(error.to_string()),
            }
            WorkerBody::query(columns, column_types, rows, 0, truncated)
        }
        Err(error) => WorkerBody::err(error.to_string()),
    }
}

fn backup(conn: &Connection, dest: &str) -> WorkerBody {
    if dest.trim().is_empty() || dest.contains('\0') {
        return WorkerBody::err("Backup destination is invalid");
    }
    if let Some(parent) = Path::new(dest).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                return WorkerBody::err(error.to_string());
            }
        }
    }
    match conn.backup(DatabaseName::Main, dest, None) {
        Ok(()) => WorkerBody::ok(),
        Err(error) => WorkerBody::err(format!("SQLite backup failed: {error}")),
    }
}

fn restore(conn: &mut Connection, src: &str) -> WorkerBody {
    if src.trim().is_empty() || src.contains('\0') {
        return WorkerBody::err("Restore source is invalid");
    }
    match conn.restore(DatabaseName::Main, src, None::<fn(_)>) {
        Ok(()) => WorkerBody::ok(),
        Err(error) => WorkerBody::err(format!("SQLite restore failed: {error}")),
    }
}

fn value_to_json(value: rusqlite::types::ValueRef<'_>, column_decl_type: Option<&str>) -> (serde_json::Value, bool) {
    match value {
        rusqlite::types::ValueRef::Null => (json!(null), false),
        rusqlite::types::ValueRef::Integer(value) => (json!(value), false),
        rusqlite::types::ValueRef::Real(value) => (json!(value), false),
        rusqlite::types::ValueRef::Text(value) => text_to_json(&String::from_utf8_lossy(value)),
        rusqlite::types::ValueRef::Blob(value) => sqlite_blob_value_to_json(value, column_decl_type),
    }
}

fn sqlite_blob_value_to_json(bytes: &[u8], column_decl_type: Option<&str>) -> (serde_json::Value, bool) {
    if is_sqlite_text_affinity(column_decl_type) {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return text_to_json(text);
        }
    }
    let truncated = bytes.len() > MAX_BLOB_BYTES;
    let encoded = &bytes[..bytes.len().min(MAX_BLOB_BYTES)];
    (json!(format!("0x{}", hex_encode(encoded))), truncated)
}

fn text_to_json(text: &str) -> (serde_json::Value, bool) {
    if text.len() <= MAX_TEXT_BYTES {
        return (json!(text), false);
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (json!(&text[..end]), true)
}

/// JSON size of a row computed without serializing it a second time. Mirrors
/// serde_json's compact encoding (escapes included) for the cell value types
/// this worker produces.
fn estimated_row_json_bytes(values: &[serde_json::Value]) -> usize {
    2 + values.len().saturating_sub(1) + values.iter().map(estimated_json_bytes).sum::<usize>()
}

fn estimated_json_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(value) => {
            if *value {
                4
            } else {
                5
            }
        }
        serde_json::Value::Number(number) => number.to_string().len(),
        serde_json::Value::String(text) => {
            2 + text
                .bytes()
                .map(|byte| match byte {
                    b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
                    0x00..=0x1f => 6,
                    _ => 1,
                })
                .sum::<usize>()
        }
        serde_json::Value::Array(items) => estimated_row_json_bytes(items),
        // Cells are never objects; fall back to an exact measurement.
        serde_json::Value::Object(_) => serde_json::to_string(value).map_or(0, |encoded| encoded.len()),
    }
}

fn is_sqlite_text_affinity(column_decl_type: Option<&str>) -> bool {
    let upper = column_decl_type.unwrap_or("").to_ascii_uppercase();
    upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT")
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::WorkerRequest;

    #[test]
    fn open_query_and_backup() {
        let dir = std::env::temp_dir().join(format!("dbx-sqlite-worker-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("app.db");
        rusqlite::Connection::open(&db).unwrap();
        let backup = dir.join("app.bak");
        let mut connection = None;
        let open =
            handle(&mut connection, WorkerRequest { id: 1, op: WorkerOp::Open { path: db.to_string_lossy().into() } });
        assert!(matches!(open.body, WorkerBody::Ok { .. }));
        let create = handle(
            &mut connection,
            WorkerRequest { id: 2, op: WorkerOp::Query { sql: "CREATE TABLE t(id INTEGER)".into(), max_rows: None } },
        );
        assert!(matches!(create.body, WorkerBody::Ok { .. }));
        let insert = handle(
            &mut connection,
            WorkerRequest { id: 3, op: WorkerOp::Query { sql: "INSERT INTO t VALUES (1)".into(), max_rows: None } },
        );
        assert!(matches!(insert.body, WorkerBody::Ok { .. }));
        let select = handle(
            &mut connection,
            WorkerRequest { id: 4, op: WorkerOp::Query { sql: "SELECT id FROM t".into(), max_rows: Some(10) } },
        );
        match select.body {
            WorkerBody::Ok { rows, .. } => assert_eq!(rows.unwrap()[0][0], json!(1)),
            WorkerBody::Err { error } => panic!("{error}"),
        }
        let backup_resp = handle(
            &mut connection,
            WorkerRequest { id: 5, op: WorkerOp::Backup { dest: backup.to_string_lossy().into() } },
        );
        assert!(matches!(backup_resp.body, WorkerBody::Ok { .. }), "{backup_resp:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn open_rejects_missing_files_instead_of_creating_them() {
        let path = std::env::temp_dir().join(format!("dbx-sqlite-worker-missing-{}.db", uuid_like()));
        let mut connection = None;
        let open = handle(
            &mut connection,
            WorkerRequest { id: 1, op: WorkerOp::Open { path: path.to_string_lossy().into() } },
        );
        match open.body {
            WorkerBody::Err { error } => assert!(error.contains("File does not exist"), "{error}"),
            other => panic!("expected missing-file error, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn multi_statement_scripts_run_every_statement() {
        let dir = std::env::temp_dir().join(format!("dbx-sqlite-worker-multi-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("app.db");
        rusqlite::Connection::open(&db).unwrap();
        let mut connection = None;
        let open =
            handle(&mut connection, WorkerRequest { id: 1, op: WorkerOp::Open { path: db.to_string_lossy().into() } });
        assert!(matches!(open.body, WorkerBody::Ok { .. }));
        let script = handle(
            &mut connection,
            WorkerRequest {
                id: 2,
                op: WorkerOp::Query {
                    sql: "CREATE TABLE t(id INTEGER); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);".into(),
                    max_rows: None,
                },
            },
        );
        assert!(matches!(script.body, WorkerBody::Ok { .. }), "{script:?}");
        let select = handle(
            &mut connection,
            WorkerRequest {
                id: 3,
                op: WorkerOp::Query { sql: "SELECT id FROM t ORDER BY id".into(), max_rows: Some(10) },
            },
        );
        match select.body {
            WorkerBody::Ok { rows, .. } => {
                let rows = rows.expect("rows");
                assert_eq!(rows, vec![vec![json!(1)], vec![json!(2)]]);
            }
            WorkerBody::Err { error } => panic!("{error}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn returning_dml_still_produces_a_result_set() {
        let dir = std::env::temp_dir().join(format!("dbx-sqlite-worker-returning-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("app.db");
        rusqlite::Connection::open(&db).unwrap();
        let mut connection = None;
        handle(&mut connection, WorkerRequest { id: 1, op: WorkerOp::Open { path: db.to_string_lossy().into() } });
        handle(
            &mut connection,
            WorkerRequest { id: 2, op: WorkerOp::Query { sql: "CREATE TABLE t(id INTEGER)".into(), max_rows: None } },
        );
        let insert = handle(
            &mut connection,
            WorkerRequest {
                id: 3,
                op: WorkerOp::Query { sql: "INSERT INTO t VALUES (7) RETURNING id".into(), max_rows: Some(10) },
            },
        );
        match insert.body {
            WorkerBody::Ok { rows, .. } => assert_eq!(rows.unwrap()[0][0], json!(7)),
            WorkerBody::Err { error } => panic!("{error}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sqlite_statement_returns_rows_classifies_scripts() {
        assert!(sqlite_statement_returns_rows("SELECT 1"));
        assert!(sqlite_statement_returns_rows("-- comment\nWITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(sqlite_statement_returns_rows("INSERT INTO t VALUES (1) RETURNING id"));
        assert!(!sqlite_statement_returns_rows("CREATE TABLE t(id INTEGER); INSERT INTO t VALUES (1);"));
        assert!(!sqlite_statement_returns_rows("INSERT INTO t VALUES (1)"));
    }

    #[test]
    fn blobs_are_hex_and_text_affinity_columns_show_as_text() {
        let dir = std::env::temp_dir().join(format!("dbx-sqlite-worker-blob-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("app.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE t (bin BLOB, note TEXT); INSERT INTO t VALUES (x'0001ab', x'6869');")
                .unwrap();
        }
        let mut connection = None;
        handle(&mut connection, WorkerRequest { id: 1, op: WorkerOp::Open { path: db.to_string_lossy().into() } });
        let select = handle(
            &mut connection,
            WorkerRequest { id: 2, op: WorkerOp::Query { sql: "SELECT bin, note FROM t".into(), max_rows: Some(10) } },
        );
        match select.body {
            WorkerBody::Ok { rows, column_types, truncated, .. } => {
                assert_eq!(column_types.as_deref(), Some(["BLOB".to_string(), "TEXT".to_string()].as_slice()));
                assert_eq!(truncated, Some(false));
                let rows = rows.expect("rows");
                assert_eq!(rows[0][0], json!("0x0001ab"));
                assert_eq!(rows[0][1], json!("hi"));
            }
            WorkerBody::Err { error } => panic!("{error}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn query_iteration_errors_are_not_reported_as_partial_success() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (value TEXT);
             INSERT INTO t VALUES ('{\"id\":1}'), ('not-json');",
        )
        .unwrap();

        match query(&conn, "SELECT json_extract(value, '$.id') FROM t ORDER BY rowid", 10) {
            WorkerBody::Err { error } => assert!(error.contains("malformed JSON"), "{error}"),
            response => panic!("expected query error, got {response:?}"),
        }
    }

    #[test]
    fn oversized_blobs_truncate_hex_output() {
        assert_eq!(sqlite_blob_value_to_json(&[0x00, 0x01, 0xab], None), (json!("0x0001ab"), false));
        let big = vec![0xFFu8; MAX_BLOB_BYTES + 1];
        let (value, truncated) = sqlite_blob_value_to_json(&big, None);
        assert!(truncated);
        let hex = value.as_str().expect("hex string");
        assert!(hex.starts_with("0x"));
        assert_eq!(hex.len(), 2 + MAX_BLOB_BYTES * 2);
    }

    #[test]
    fn malformed_request_lines_get_an_error_response_and_the_worker_keeps_serving() {
        let input = b"not json\n{\"id\":9,\"op\":\"no_such_op\"}\n\xff\xfe\n{\"id\":10,\"op\":\"ping\"}\n";
        let mut output = Vec::new();
        serve(&input[..], &mut output).expect("serve must not fail on bad lines");
        let responses = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<WorkerResponse>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 4);
        assert_eq!(responses[0].id, 0);
        assert!(matches!(&responses[0].body, WorkerBody::Err { error } if error.contains("invalid worker request")));
        assert_eq!(responses[1].id, 9, "the id of a well-formed line with an unknown op is recovered");
        assert!(matches!(responses[1].body, WorkerBody::Err { .. }));
        assert_eq!(responses[2].id, 0);
        assert_eq!(responses[3], WorkerResponse { id: 10, body: WorkerBody::pong() });
    }

    #[test]
    fn oversized_text_is_truncated_on_a_char_boundary_and_flagged() {
        assert_eq!(text_to_json("hi"), (json!("hi"), false));
        let big = "é".repeat(MAX_TEXT_BYTES / 2 + 1);
        let (value, truncated) = text_to_json(&big);
        assert!(truncated);
        let text = value.as_str().unwrap();
        assert!(text.len() <= MAX_TEXT_BYTES);
        assert!(text.chars().all(|ch| ch == 'é'));

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        match query(&conn, &format!("SELECT '{}' AS big, 1 AS small", "x".repeat(MAX_TEXT_BYTES + 10)), 10) {
            WorkerBody::Ok { rows, truncated, .. } => {
                assert_eq!(truncated, Some(true));
                assert_eq!(rows.unwrap()[0][0].as_str().unwrap().len(), MAX_TEXT_BYTES);
            }
            WorkerBody::Err { error } => panic!("{error}"),
        }
    }

    #[test]
    fn response_size_limit_applies_to_the_first_row() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let columns = (0..10).map(|index| format!("zeroblob({MAX_BLOB_BYTES}) AS b{index}")).collect::<Vec<_>>();
        match query(&conn, &format!("SELECT {}", columns.join(", ")), 10) {
            WorkerBody::Err { error } => assert!(error.contains("response limit"), "{error}"),
            other => panic!("expected the oversized first row to be rejected, got {} bytes", format!("{other:?}").len()),
        }
    }

    #[test]
    fn response_size_limit_truncates_later_rows() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let sql = format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 40) SELECT zeroblob({MAX_BLOB_BYTES}) FROM n"
        );
        match query(&conn, &sql, 1_000) {
            WorkerBody::Ok { rows, truncated, .. } => {
                let rows = rows.unwrap();
                assert_eq!(truncated, Some(true));
                assert!(!rows.is_empty() && rows.len() < 40, "{}", rows.len());
                assert!(estimated_row_json_bytes(&rows.concat()) <= MAX_RESPONSE_JSON_BYTES);
            }
            WorkerBody::Err { error } => panic!("{error}"),
        }
    }

    #[test]
    fn row_size_estimate_matches_serialized_length() {
        let row = vec![
            json!(null),
            json!(true),
            json!(-12.5),
            json!(1234567890123_i64),
            json!("plain"),
            json!("quote\" back\\ nl\n tab\t ctl\u{1} é"),
            json!([1, "a"]),
        ];
        assert_eq!(estimated_row_json_bytes(&row), serde_json::to_vec(&row).unwrap().len());
        assert_eq!(estimated_row_json_bytes(&[]), 2);
    }

    fn uuid_like() -> String {
        format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        )
    }
}
