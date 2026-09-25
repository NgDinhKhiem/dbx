use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};
use sqlparser::ast::{
    visit_expressions, BinaryOperator, Expr, FromTable, OnConflictAction, OnInsert, Query, SetExpr, SqliteOnConflict,
    Statement, TableFactor, UnaryOperator, Value, Visit, Visitor,
};
use sqlparser::dialect::{
    ClickHouseDialect, DuckDbDialect, GenericDialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::models::connection::DatabaseType;

/// SQL risk level for agent tool safety classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SqlRisk {
    /// SELECT, SHOW, DESCRIBE, EXPLAIN, WITH (pure read CTE)
    ReadOnly,
    /// INSERT, UPDATE, DELETE, MERGE, REPLACE, CALL/EXEC
    Write,
    /// CREATE, ALTER, DROP, TRUNCATE, GRANT, REVOKE
    Ddl,
    /// BEGIN, COMMIT, ROLLBACK should not be issued by agent
    Transaction,
}

impl std::fmt::Display for SqlRisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqlRisk::ReadOnly => write!(f, "read-only"),
            SqlRisk::Write => write!(f, "write"),
            SqlRisk::Ddl => write!(f, "DDL"),
            SqlRisk::Transaction => write!(f, "transaction"),
        }
    }
}

/// Normalize database dialect string to a canonical form for sqlparser.
/// Mirrors the logic in `sql_analysis::normalize_dialect`.
fn normalize_dialect(dialect: &str) -> &'static str {
    match dialect.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" | "redshift" | "opengauss" | "gaussdb" | "kingbase" | "highgo" | "uxdb"
        | "vastbase" | "kwdb" => "postgres",
        "mysql" | "mariadb" | "doris" | "starrocks" | "manticoresearch" | "oceanbase" => "mysql",
        "sqlite" => "sqlite",
        "sqlserver" | "mssql" => "sqlserver",
        "clickhouse" => "clickhouse",
        "duckdb" => "duckdb",
        _ => "generic",
    }
}

/// Resolve dialect string to a sqlparser Dialect trait object.
fn resolve_dialect(dialect: &str) -> Box<dyn sqlparser::dialect::Dialect> {
    match dialect {
        "postgres" => Box::new(PostgreSqlDialect {}),
        "mysql" => Box::new(MySqlDialect {}),
        "sqlite" => Box::new(SQLiteDialect {}),
        "sqlserver" => Box::new(MsSqlDialect {}),
        "clickhouse" => Box::new(ClickHouseDialect {}),
        "duckdb" => Box::new(DuckDbDialect {}),
        _ => Box::new(GenericDialect {}),
    }
}

/// Classify a single SQL statement into a risk level using AST analysis.
fn classify_statement(stmt: &Statement, detect_select_into: bool) -> SqlRisk {
    match stmt {
        // Pure reads
        Statement::Query(query) => {
            if query_is_write_capable(query, detect_select_into) {
                SqlRisk::Write
            } else {
                SqlRisk::ReadOnly
            }
        }
        Statement::Explain { analyze, statement, .. } => {
            if *analyze {
                classify_statement(statement, detect_select_into)
            } else {
                SqlRisk::ReadOnly
            }
        }
        Statement::ExplainTable { .. } => SqlRisk::ReadOnly,

        // Show/Describe variants
        stmt if is_show_read_only_statement(stmt) => SqlRisk::ReadOnly,

        // Write operations
        Statement::Insert { .. } | Statement::Update { .. } | Statement::Delete { .. } | Statement::Merge { .. } => {
            SqlRisk::Write
        }

        // DDL operations
        Statement::CreateTable { .. }
        | Statement::CreateView { .. }
        | Statement::CreateIndex { .. }
        | Statement::CreateSchema { .. }
        | Statement::CreateSequence { .. }
        | Statement::CreateRole { .. }
        | Statement::CreateType { .. }
        | Statement::AlterTable { .. }
        | Statement::AlterIndex { .. }
        | Statement::AlterView { .. }
        | Statement::Drop { .. }
        | Statement::Truncate { .. } => SqlRisk::Ddl,

        // Grant/Revoke
        Statement::Grant { .. } | Statement::Revoke { .. } => SqlRisk::Ddl,

        // Transaction control
        Statement::StartTransaction { .. } | Statement::Commit { .. } | Statement::Rollback { .. } => {
            SqlRisk::Transaction
        }

        // COPY FROM mutates data; keep COPY conservative because sqlparser does
        // not expose enough dialect-specific direction detail here.
        Statement::Copy { .. } => SqlRisk::Write,

        // SQLite/DuckDB PRAGMA statements can mutate database/session state.
        Statement::Pragma { .. } => SqlRisk::Write,

        // Catch-all: conservative write classification
        _ => SqlRisk::Write,
    }
}

/// SHOW/DESCRIBE-style statement variants sqlparser models as reads. Shared by
/// the risk classifier and the manual-transaction proof (#9018) so the two
/// cannot drift; unparseable SHOW variants fail closed upstream in the proof
/// (parse error) and to the keyword fallback in the risk classifier.
fn is_show_read_only_statement(statement: &Statement) -> bool {
    matches!(
        statement,
        Statement::ShowTables { .. }
            | Statement::ShowColumns { .. }
            | Statement::ShowCatalogs { .. }
            | Statement::ShowDatabases { .. }
            | Statement::ShowSchemas { .. }
            | Statement::ShowViews { .. }
            | Statement::ShowFunctions { .. }
            | Statement::ShowCreate { .. }
            | Statement::ShowVariable { .. }
            | Statement::ShowVariables { .. }
            | Statement::ShowStatus { .. }
            | Statement::ShowProcessList { .. }
            | Statement::ShowCharset(_)
            | Statement::ShowObjects(_)
            | Statement::ShowCollation { .. }
    )
}

fn statement_is_dangerous(stmt: &Statement, detect_select_into: bool) -> bool {
    match stmt {
        // `USE` only changes connection-local state. MCP separately requires
        // a pinned session for it, so it does not need dangerous-SQL approval.
        Statement::Use(_) => false,
        Statement::Query(query) => query_is_dangerous(query, detect_select_into),
        Statement::Insert(insert) => {
            insert.replace_into
                || insert.overwrite
                || matches!(insert.or, Some(SqliteOnConflict::Replace))
                || insert.on.as_ref().is_some_and(|on| match on {
                    OnInsert::DuplicateKeyUpdate(_) => true,
                    OnInsert::OnConflict(conflict) => matches!(conflict.action, OnConflictAction::DoUpdate(_)),
                    _ => true,
                })
                || insert.source.as_ref().is_some_and(|query| !matches!(query.body.as_ref(), SetExpr::Values(_)))
        }
        Statement::Update(update) => {
            !update.table.joins.is_empty()
                || update.from.is_some()
                || matches!(update.or, Some(SqliteOnConflict::Replace))
                || update.selection.as_ref().is_none_or(predicate_is_obviously_unbounded)
        }
        Statement::Delete(delete) => {
            let from = match &delete.from {
                FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from) => from,
            };
            !delete.tables.is_empty()
                || delete.using.is_some()
                || from.len() != 1
                || from.first().is_none_or(|table| !table.joins.is_empty())
                || delete.selection.as_ref().is_none_or(predicate_is_obviously_unbounded)
        }
        Statement::Merge { .. } => true,
        Statement::Explain { analyze: true, statement, .. } => statement_is_dangerous(statement, detect_select_into),
        _ => !matches!(classify_statement(stmt, detect_select_into), SqlRisk::ReadOnly),
    }
}

fn predicate_is_obviously_unbounded(expr: &Expr) -> bool {
    if expr_contains_subquery(expr) {
        return true;
    }
    if !expr_contains_column_reference(expr) {
        return true;
    }

    match expr {
        Expr::Nested(expr) => predicate_is_obviously_unbounded(expr),
        Expr::Value(value) => value_is_truthy(&value.value),
        Expr::UnaryOp { op: UnaryOperator::Not, expr } => predicate_is_obviously_false(expr),
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
            predicate_is_obviously_unbounded(left) && predicate_is_obviously_unbounded(right)
        }
        Expr::BinaryOp { left, op: BinaryOperator::Or, right } => {
            or_contains_conjunctive_branch(expr)
                || or_contains_complementary_null_checks(expr)
                || or_contains_complementary_comparisons(expr)
                || or_contains_complementary_in_lists(expr)
                || or_contains_complementary_between_checks(expr)
                || predicate_is_obviously_unbounded(left)
                || predicate_is_obviously_unbounded(right)
        }
        Expr::BinaryOp { left, op, right } if is_comparison_operator(op) => constant_comparison_truth(left, op, right)
            .unwrap_or_else(|| {
                left == right
                    && matches!(
                        op,
                        BinaryOperator::Eq | BinaryOperator::GtEq | BinaryOperator::LtEq | BinaryOperator::Spaceship
                    )
            }),
        Expr::IsNotDistinctFrom(left, right) => strip_nested_expr(left) == strip_nested_expr(right),
        Expr::IsTrue(expr) | Expr::IsNotFalse(expr) => predicate_is_obviously_unbounded(expr),
        Expr::Like { negated: false, any: false, expr, pattern, escape_char: None }
        | Expr::ILike { negated: false, any: false, expr, pattern, escape_char: None } => {
            expr_contains_column_reference(expr) && like_pattern_matches_all(pattern)
        }
        _ => false,
    }
}

fn or_contains_conjunctive_branch(expr: &Expr) -> bool {
    match strip_nested_expr(expr) {
        Expr::BinaryOp { left, op: BinaryOperator::Or, right } => {
            or_contains_conjunctive_branch(left) || or_contains_conjunctive_branch(right)
        }
        Expr::BinaryOp { op: BinaryOperator::And, .. } => true,
        _ => false,
    }
}

fn expr_contains_column_reference(expr: &Expr) -> bool {
    visit_expressions(expr, |expr| {
        let is_column = match expr {
            Expr::Identifier(identifier) => !is_sql_value_keyword(&identifier.value),
            Expr::CompoundIdentifier(_) => true,
            _ => false,
        };
        if is_column {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_break()
}

fn is_sql_value_keyword(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "current_catalog"
            | "current_date"
            | "current_role"
            | "current_schema"
            | "current_time"
            | "current_timestamp"
            | "current_user"
            | "localtime"
            | "localtimestamp"
            | "session_user"
            | "system_user"
            | "user"
    )
}

fn expr_contains_subquery(expr: &Expr) -> bool {
    visit_expressions(expr, |expr| {
        if matches!(expr, Expr::InSubquery { .. } | Expr::Exists { .. } | Expr::Subquery(_)) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_break()
}

fn or_contains_complementary_null_checks(expr: &Expr) -> bool {
    let mut checks = Vec::new();
    collect_or_null_checks(expr, &mut checks);
    checks.iter().enumerate().any(|(index, (candidate, negated))| {
        checks[index + 1..].iter().any(|(other, other_negated)| candidate == other && negated != other_negated)
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PredicateComparisonOperator {
    Eq,
    NotEq,
    Gt,
    GtEq,
    Lt,
    LtEq,
}

struct PredicateComparisonCheck<'a> {
    left: &'a Expr,
    right: &'a Expr,
    operator: PredicateComparisonOperator,
}

fn or_contains_complementary_comparisons(expr: &Expr) -> bool {
    let mut checks = Vec::new();
    collect_or_comparison_checks(expr, &mut checks);
    checks.iter().enumerate().any(|(index, candidate)| {
        checks[index + 1..].iter().any(|other| {
            candidate.left == other.left
                && candidate.right == other.right
                && comparison_operators_are_complementary(candidate.operator, other.operator)
        })
    })
}

fn collect_or_comparison_checks<'a>(expr: &'a Expr, checks: &mut Vec<PredicateComparisonCheck<'a>>) {
    let expr = strip_nested_expr(expr);
    if let Expr::BinaryOp { left, op: BinaryOperator::Or, right } = expr {
        collect_or_comparison_checks(left, checks);
        collect_or_comparison_checks(right, checks);
    } else if let Some(check) = comparison_check(expr, false) {
        checks.push(check);
    }
}

fn comparison_check(expr: &Expr, outer_negated: bool) -> Option<PredicateComparisonCheck<'_>> {
    let expr = strip_nested_expr(expr);
    if let Expr::UnaryOp { op: UnaryOperator::Not, expr } = expr {
        return comparison_check(expr, !outer_negated);
    }
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    if !expr_contains_column_reference(expr) {
        return None;
    }

    let mut operator = predicate_comparison_operator(op)?;
    if outer_negated {
        operator = complementary_comparison_operator(operator);
    }
    let (left, right) = (strip_nested_expr(left), strip_nested_expr(right));
    if left <= right {
        Some(PredicateComparisonCheck { left, right, operator })
    } else {
        Some(PredicateComparisonCheck { left: right, right: left, operator: reverse_comparison_operator(operator) })
    }
}

fn predicate_comparison_operator(operator: &BinaryOperator) -> Option<PredicateComparisonOperator> {
    match operator {
        BinaryOperator::Eq | BinaryOperator::Spaceship => Some(PredicateComparisonOperator::Eq),
        BinaryOperator::NotEq => Some(PredicateComparisonOperator::NotEq),
        BinaryOperator::Gt => Some(PredicateComparisonOperator::Gt),
        BinaryOperator::GtEq => Some(PredicateComparisonOperator::GtEq),
        BinaryOperator::Lt => Some(PredicateComparisonOperator::Lt),
        BinaryOperator::LtEq => Some(PredicateComparisonOperator::LtEq),
        _ => None,
    }
}

fn complementary_comparison_operator(operator: PredicateComparisonOperator) -> PredicateComparisonOperator {
    match operator {
        PredicateComparisonOperator::Eq => PredicateComparisonOperator::NotEq,
        PredicateComparisonOperator::NotEq => PredicateComparisonOperator::Eq,
        PredicateComparisonOperator::Gt => PredicateComparisonOperator::LtEq,
        PredicateComparisonOperator::GtEq => PredicateComparisonOperator::Lt,
        PredicateComparisonOperator::Lt => PredicateComparisonOperator::GtEq,
        PredicateComparisonOperator::LtEq => PredicateComparisonOperator::Gt,
    }
}

fn reverse_comparison_operator(operator: PredicateComparisonOperator) -> PredicateComparisonOperator {
    match operator {
        PredicateComparisonOperator::Eq | PredicateComparisonOperator::NotEq => operator,
        PredicateComparisonOperator::Gt => PredicateComparisonOperator::Lt,
        PredicateComparisonOperator::GtEq => PredicateComparisonOperator::LtEq,
        PredicateComparisonOperator::Lt => PredicateComparisonOperator::Gt,
        PredicateComparisonOperator::LtEq => PredicateComparisonOperator::GtEq,
    }
}

fn comparison_operators_are_complementary(
    left: PredicateComparisonOperator,
    right: PredicateComparisonOperator,
) -> bool {
    complementary_comparison_operator(left) == right
}

struct PredicateInListCheck<'a> {
    expr: &'a Expr,
    list: Vec<&'a Expr>,
    negated: bool,
}

fn or_contains_complementary_in_lists(expr: &Expr) -> bool {
    let mut checks = Vec::new();
    collect_or_in_list_checks(expr, &mut checks);
    checks.iter().enumerate().any(|(index, candidate)| {
        checks[index + 1..].iter().any(|other| {
            candidate.expr == other.expr && candidate.list == other.list && candidate.negated != other.negated
        })
    })
}

fn collect_or_in_list_checks<'a>(expr: &'a Expr, checks: &mut Vec<PredicateInListCheck<'a>>) {
    let expr = strip_nested_expr(expr);
    if let Expr::BinaryOp { left, op: BinaryOperator::Or, right } = expr {
        collect_or_in_list_checks(left, checks);
        collect_or_in_list_checks(right, checks);
    } else if let Some(check) = in_list_check(expr, false) {
        checks.push(check);
    }
}

fn in_list_check(expr: &Expr, outer_negated: bool) -> Option<PredicateInListCheck<'_>> {
    match strip_nested_expr(expr) {
        Expr::UnaryOp { op: UnaryOperator::Not, expr } => in_list_check(expr, !outer_negated),
        Expr::InList { expr, list, negated } if expr_contains_column_reference(expr) && !list.is_empty() => {
            let mut normalized_list = list.iter().map(strip_nested_expr).collect::<Vec<_>>();
            if normalized_list.iter().all(|item| matches!(strip_nested_expr(item), Expr::Value(_))) {
                normalized_list.sort_unstable();
                normalized_list.dedup();
            }
            Some(PredicateInListCheck {
                expr: strip_nested_expr(expr),
                list: normalized_list,
                negated: *negated != outer_negated,
            })
        }
        _ => None,
    }
}

struct PredicateBetweenCheck<'a> {
    expr: &'a Expr,
    low: &'a Expr,
    high: &'a Expr,
    negated: bool,
}

fn or_contains_complementary_between_checks(expr: &Expr) -> bool {
    let mut checks = Vec::new();
    collect_or_between_checks(expr, &mut checks);
    checks.iter().enumerate().any(|(index, candidate)| {
        checks[index + 1..].iter().any(|other| {
            candidate.expr == other.expr
                && candidate.low == other.low
                && candidate.high == other.high
                && candidate.negated != other.negated
        })
    })
}

fn collect_or_between_checks<'a>(expr: &'a Expr, checks: &mut Vec<PredicateBetweenCheck<'a>>) {
    let expr = strip_nested_expr(expr);
    if let Expr::BinaryOp { left, op: BinaryOperator::Or, right } = expr {
        collect_or_between_checks(left, checks);
        collect_or_between_checks(right, checks);
    } else if let Some(check) = between_check(expr, false) {
        checks.push(check);
    }
}

fn between_check(expr: &Expr, outer_negated: bool) -> Option<PredicateBetweenCheck<'_>> {
    match strip_nested_expr(expr) {
        Expr::UnaryOp { op: UnaryOperator::Not, expr } => between_check(expr, !outer_negated),
        Expr::Between { expr, negated, low, high } if expr_contains_column_reference(expr) => {
            Some(PredicateBetweenCheck {
                expr: strip_nested_expr(expr),
                low: strip_nested_expr(low),
                high: strip_nested_expr(high),
                negated: *negated != outer_negated,
            })
        }
        _ => None,
    }
}

fn like_pattern_matches_all(expr: &Expr) -> bool {
    matches!(strip_nested_expr(expr), Expr::Value(value) if matches!(&value.value, Value::SingleQuotedString(pattern) if !pattern.is_empty() && pattern.chars().all(|character| character == '%')))
}

fn collect_or_null_checks<'a>(expr: &'a Expr, checks: &mut Vec<(&'a Expr, bool)>) {
    let expr = strip_nested_expr(expr);
    if let Expr::BinaryOp { left, op: BinaryOperator::Or, right } = expr {
        collect_or_null_checks(left, checks);
        collect_or_null_checks(right, checks);
    } else if let Some(check) = null_check(expr) {
        checks.push(check);
    }
}

fn null_check(expr: &Expr) -> Option<(&Expr, bool)> {
    null_check_with_negation(expr, false)
}

fn null_check_with_negation(expr: &Expr, outer_negated: bool) -> Option<(&Expr, bool)> {
    match strip_nested_expr(expr) {
        Expr::UnaryOp { op: UnaryOperator::Not, expr } => null_check_with_negation(expr, !outer_negated),
        Expr::IsNull(expr) => Some((strip_nested_expr(expr), outer_negated)),
        Expr::IsNotNull(expr) => Some((strip_nested_expr(expr), !outer_negated)),
        _ => None,
    }
}

fn strip_nested_expr(mut expr: &Expr) -> &Expr {
    while let Expr::Nested(inner) = expr {
        expr = inner;
    }
    expr
}

fn predicate_is_obviously_false(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(expr) => predicate_is_obviously_false(expr),
        Expr::Value(value) => value_is_falsy(&value.value),
        Expr::UnaryOp { op: UnaryOperator::Not, expr } => predicate_is_obviously_unbounded(expr),
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
            predicate_is_obviously_false(left) || predicate_is_obviously_false(right)
        }
        Expr::BinaryOp { left, op: BinaryOperator::Or, right } => {
            predicate_is_obviously_false(left) && predicate_is_obviously_false(right)
        }
        Expr::BinaryOp { left, op, right } if is_comparison_operator(op) => {
            constant_comparison_truth(left, op, right).is_some_and(|result| !result)
                || (left == right && matches!(op, BinaryOperator::NotEq | BinaryOperator::Gt | BinaryOperator::Lt))
        }
        Expr::IsDistinctFrom(left, right) => strip_nested_expr(left) == strip_nested_expr(right),
        Expr::IsFalse(expr) | Expr::IsNotTrue(expr) => predicate_is_obviously_unbounded(expr),
        _ => false,
    }
}

fn is_comparison_operator(operator: &BinaryOperator) -> bool {
    matches!(
        operator,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq
            | BinaryOperator::Spaceship
    )
}

fn constant_comparison_truth(left: &Expr, operator: &BinaryOperator, right: &Expr) -> Option<bool> {
    let (Expr::Value(left), Expr::Value(right)) = (left, right) else {
        return None;
    };
    match operator {
        BinaryOperator::Eq | BinaryOperator::Spaceship => Some(left.value == right.value),
        BinaryOperator::NotEq => Some(left.value != right.value),
        BinaryOperator::Gt | BinaryOperator::Lt | BinaryOperator::GtEq | BinaryOperator::LtEq => {
            let (Value::Number(left, _), Value::Number(right, _)) = (&left.value, &right.value) else {
                return None;
            };
            let left = left.parse::<f64>().ok()?;
            let right = right.parse::<f64>().ok()?;
            Some(match operator {
                BinaryOperator::Gt => left > right,
                BinaryOperator::Lt => left < right,
                BinaryOperator::GtEq => left >= right,
                BinaryOperator::LtEq => left <= right,
                _ => unreachable!(),
            })
        }
        _ => None,
    }
}

fn value_is_truthy(value: &Value) -> bool {
    match value {
        Value::Boolean(value) => *value,
        Value::Number(value, _) => value.parse::<f64>().is_ok_and(|number| number != 0.0),
        _ => false,
    }
}

fn value_is_falsy(value: &Value) -> bool {
    match value {
        Value::Boolean(value) => !*value,
        Value::Number(value, _) => value.parse::<f64>().is_ok_and(|number| number == 0.0),
        Value::Null => true,
        _ => false,
    }
}

/// Functions whose call from an otherwise read-only statement has effects outside the statement's
/// own result: data or sequence writes, locks, session/server control, file-system or network
/// access, or remote execution. Matched case-insensitively on the last name part, so a schema
/// qualified call (`pg_catalog.pg_terminate_backend(...)`) is caught as well. KingbaseES `sys_*`
/// spellings of PostgreSQL `pg_*` functions are matched through the `pg_` name.
const SIDE_EFFECT_SELECT_FUNCTIONS: &[&str] = &[
    // PostgreSQL family
    "cursor_to_xml",
    "http",
    "loread",
    "lowrite",
    "nextval",
    "pg_cancel_backend",
    "pg_extension_config_dump",
    "pg_log_backend_memory_contexts",
    "pg_logdir_ls",
    "pg_logical_emit_message",
    "pg_notify",
    "pg_promote",
    "pg_reload_conf",
    "pg_replication_slot_advance",
    "pg_rotate_logfile",
    "pg_rotate_logfile_old",
    "pg_signal_backend",
    "pg_sleep",
    "pg_sleep_for",
    "pg_sleep_until",
    "pg_start_backup",
    "pg_stat_file",
    "pg_stat_statements_reset",
    "pg_stop_backup",
    "pg_switch_wal",
    "pg_switch_xlog",
    "pg_terminate_backend",
    "query_to_xml",
    "query_to_xml_and_xmlschema",
    "query_to_xmlschema",
    "set_config",
    "setval",
    // MySQL family
    "benchmark",
    "get_lock",
    "load_file",
    "master_pos_wait",
    "release_all_locks",
    "release_lock",
    "sleep",
    "source_pos_wait",
    "sys_eval",
    "sys_exec",
    "wait_for_executed_gtid_set",
    "wait_until_sql_thread_after_gtids",
    // SQL Server
    "opendatasource",
    "openquery",
    "openrowset",
    // SQLite (loadable-extension and CLI file helpers)
    "edit",
    "fts3_tokenizer",
    "load_extension",
    "readfile",
    "writefile",
    // Oracle
    "httpuritype",
];

/// Name prefixes with the same meaning as [`SIDE_EFFECT_SELECT_FUNCTIONS`]: PostgreSQL admin,
/// replication, large-object, file and `dblink` families, MySQL locking/keyring/audit
/// services, and Oracle `DBMS_*` / `UTL_*` packages (see [`oracle_package_call_is_read_only`]).
const SIDE_EFFECT_FUNCTION_PREFIXES: &[&str] = &[
    "audit_log_",
    "binary_upgrade_",
    "dblink",
    "dbms_",
    "http_",
    "keyring_",
    "lo_",
    "pg_advisory_",
    "pg_backup_",
    "pg_clear_",
    "pg_copy_",
    "pg_create_",
    "pg_drop_",
    "pg_file_",
    "pg_import_",
    "pg_logical_slot_",
    "pg_ls_",
    "pg_read_",
    "pg_replication_origin_",
    "pg_restore_",
    "pg_set_",
    "pg_stat_reset",
    "pg_try_advisory_",
    "pg_wal_replay_",
    "pg_write",
    "pg_xlog_replay_",
    "service_get_",
    "service_release_",
    "utl_",
    "version_tokens_",
    "xp_",
];

/// Table functions (`FROM name(...)`) that read or write outside the database: ClickHouse's
/// file/URL/remote-server and script-execution table functions.
const SIDE_EFFECT_TABLE_FUNCTIONS: &[&str] = &[
    "azureblobstorage",
    "executable",
    "file",
    "gcs",
    "hdfs",
    "hdfscluster",
    "jdbc",
    "mysql",
    "odbc",
    "postgresql",
    "remote",
    "remotesecure",
    "s3",
    "s3cluster",
    "url",
    "urlcluster",
];

/// Oracle packages whose members are side-effect free (or only touch the caller's own session in
/// a harmless way). Every other `DBMS_*` / `UTL_*` call is treated as side-effecting.
fn oracle_package_call_is_read_only(package: &str, member: Option<&str>) -> bool {
    match package {
        "dbms_assert" | "dbms_crypto" | "dbms_metadata" | "dbms_obfuscation_toolkit" | "dbms_random" => true,
        "dbms_lob" => member.is_some_and(|member| {
            matches!(
                member,
                "compare"
                    | "get_storage_limit"
                    | "getchunksize"
                    | "getlength"
                    | "instr"
                    | "isopen"
                    | "istemporary"
                    | "substr"
            )
        }),
        _ => false,
    }
}

fn is_side_effect_function_name(name: &str) -> bool {
    SIDE_EFFECT_SELECT_FUNCTIONS.contains(&name) || SIDE_EFFECT_FUNCTION_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Whether a (possibly qualified) function name is a known side-effecting call.
fn is_side_effect_function(name_parts: &[String]) -> bool {
    let lowered = name_parts.iter().map(|part| part.to_ascii_lowercase()).collect::<Vec<_>>();
    let Some(last) = lowered.last() else {
        return false;
    };
    // Oracle packages: `DBMS_LOCK.SLEEP`, `SYS.UTL_HTTP.REQUEST`, ...
    if let Some(index) = lowered.iter().position(|part| part.starts_with("dbms_") || part.starts_with("utl_")) {
        return !oracle_package_call_is_read_only(&lowered[index], lowered.get(index + 1).map(String::as_str));
    }
    // pg_cron job management (`cron.schedule(...)`).
    if lowered.len() >= 2 && lowered[lowered.len() - 2] == "cron" {
        return true;
    }
    is_side_effect_function_name(last)
        || last.strip_prefix("sys_").is_some_and(|rest| is_side_effect_function_name(&format!("pg_{rest}")))
}

fn object_name_parts(name: &sqlparser::ast::ObjectName) -> Vec<String> {
    name.0
        .iter()
        .map(|part| part.as_ident().map(|ident| ident.value.clone()).unwrap_or_else(|| part.to_string()))
        .collect()
}

/// Name of a table-valued function call in `FROM`, if the table factor is one.
fn table_factor_function_name(table_factor: &TableFactor) -> Option<&sqlparser::ast::ObjectName> {
    match table_factor {
        TableFactor::Table { name, args: Some(_), .. } | TableFactor::Function { name, .. } => Some(name),
        _ => None,
    }
}

fn is_side_effect_table_function(name_parts: &[String]) -> bool {
    is_side_effect_function(name_parts)
        || name_parts
            .last()
            .is_some_and(|name| SIDE_EFFECT_TABLE_FUNCTIONS.contains(&name.to_ascii_lowercase().as_str()))
}

fn query_is_write_capable(query: &Query, detect_select_into: bool) -> bool {
    query
        .with
        .as_ref()
        .is_some_and(|with| with.cte_tables.iter().any(|cte| query_is_write_capable(&cte.query, detect_select_into)))
        || set_expr_is_write_capable(&query.body, detect_select_into)
        || !query.locks.is_empty()
        || query_calls_known_side_effect_function(query)
}

fn set_expr_is_write_capable(expr: &SetExpr, detect_select_into: bool) -> bool {
    match expr {
        SetExpr::Select(select) => detect_select_into && select.into.is_some(),
        SetExpr::Query(query) => query_is_write_capable(query, detect_select_into),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_is_write_capable(left, detect_select_into) || set_expr_is_write_capable(right, detect_select_into)
        }
        SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_) => true,
        SetExpr::Values(_) | SetExpr::Table(_) => false,
    }
}

fn query_is_dangerous(query: &Query, detect_select_into: bool) -> bool {
    query
        .with
        .as_ref()
        .is_some_and(|with| with.cte_tables.iter().any(|cte| query_is_dangerous(&cte.query, detect_select_into)))
        || set_expr_is_dangerous(&query.body, detect_select_into)
        || !query.locks.is_empty()
        || query_calls_known_side_effect_function(query)
}

fn set_expr_is_dangerous(expr: &SetExpr, detect_select_into: bool) -> bool {
    match expr {
        SetExpr::Select(select) => detect_select_into && select.into.is_some(),
        SetExpr::Query(query) => query_is_dangerous(query, detect_select_into),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_is_dangerous(left, detect_select_into) || set_expr_is_dangerous(right, detect_select_into)
        }
        SetExpr::Insert(statement)
        | SetExpr::Update(statement)
        | SetExpr::Delete(statement)
        | SetExpr::Merge(statement) => statement_is_dangerous(statement, detect_select_into),
        SetExpr::Values(_) | SetExpr::Table(_) => false,
    }
}

/// Full-statement walk (the `visitor` feature also reaches FROM subqueries, LATERAL derived
/// tables and table functions, which `visit_expressions` skips).
fn query_calls_known_side_effect_function(query: &Query) -> bool {
    let mut visitor = SideEffectCallVisitor;
    query.visit(&mut visitor).is_break()
}

struct SideEffectCallVisitor;

impl Visitor for SideEffectCallVisitor {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if let Expr::Function(function) = expr {
            if is_side_effect_function(&object_name_parts(&function.name)) {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<()> {
        if table_factor_function_name(table_factor)
            .is_some_and(|name| is_side_effect_table_function(&object_name_parts(name)))
        {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }
}

/// One `name(` / `schema.name(` call found by the token-level scan.
struct TokenFunctionCall {
    parts: Vec<String>,
    quoted: bool,
}

/// Token-level function-call scan for SQL the parser rejects. Returns `None` when the text
/// cannot even be tokenized.
fn function_calls_in_tokens(sql: &str, dialect: &dyn sqlparser::dialect::Dialect) -> Option<Vec<TokenFunctionCall>> {
    let tokens = Tokenizer::new(dialect, sql).tokenize().ok()?;
    let significant = tokens.iter().filter(|token| !matches!(token, Token::Whitespace(_))).collect::<Vec<_>>();
    let mut calls = Vec::new();
    for (index, token) in significant.iter().enumerate() {
        let Token::Word(word) = token else {
            continue;
        };
        if !matches!(significant.get(index + 1), Some(Token::LParen)) {
            continue;
        }
        let mut parts = vec![word.value.clone()];
        let mut cursor = index;
        while cursor >= 2 {
            match (significant[cursor - 1], significant[cursor - 2]) {
                (Token::Period, Token::Word(qualifier)) => {
                    parts.insert(0, qualifier.value.clone());
                    cursor -= 2;
                }
                _ => break,
            }
        }
        calls.push(TokenFunctionCall { parts, quoted: word.quote_style.is_some() });
    }
    Some(calls)
}

/// Parse-failure fallback of the side-effect denylist.
fn sql_tokens_call_side_effect_function(sql: &str, dialect: &dyn sqlparser::dialect::Dialect) -> bool {
    function_calls_in_tokens(sql, dialect)
        .is_some_and(|calls| calls.iter().any(|call| is_side_effect_table_function(&call.parts)))
}

/// Classify SQL risk using sqlparser AST analysis.
///
/// If parsing fails (non-standard SQL, non-SQL databases), falls back to
/// keyword-based `query_execution_sql::is_write_sql()`.
///
/// Multi-statement input: returns the highest risk level across all statements.
pub fn classify_sql_risk(sql: &str, dialect: &str) -> Result<SqlRisk, String> {
    let normalized = normalize_dialect(dialect);
    classify_sql_risk_with_database(sql, normalized, None)
}

/// Classify SQL risk using both the parser dialect and the concrete database
/// type so dialect-specific write forms cannot be mistaken for read queries.
pub fn classify_sql_risk_for_database(sql: &str, database_type: DatabaseType) -> Result<SqlRisk, String> {
    if let Some(risk) = crate::query_execution_sql::classify_search_engine_query_risk(sql, database_type) {
        return Ok(match risk {
            crate::query_execution_sql::SearchEngineQueryRisk::ReadOnly => SqlRisk::ReadOnly,
            crate::query_execution_sql::SearchEngineQueryRisk::Write => SqlRisk::Write,
            crate::query_execution_sql::SearchEngineQueryRisk::Dangerous => SqlRisk::Ddl,
        });
    }
    let database_type_name = format!("{database_type:?}");
    let normalized = normalize_dialect(&database_type_name);
    classify_sql_risk_with_database(sql, normalized, Some(database_type))
}

/// Return whether MCP must require the central dangerous-operation permission.
/// Parse failures fail closed for writes. Safe-write mode permits plain INSERT
/// and single-table UPDATE/DELETE statements with an effective predicate;
/// broader or opaque mutations require central high-risk permission.
pub fn is_dangerous_sql_for_database(sql: &str, database_type: DatabaseType) -> bool {
    if let Some(risk) = crate::query_execution_sql::classify_search_engine_query_risk(sql, database_type) {
        return risk == crate::query_execution_sql::SearchEngineQueryRisk::Dangerous;
    }
    let database_type_name = format!("{database_type:?}");
    let normalized = normalize_dialect(&database_type_name);
    let parser_dialect = resolve_dialect(normalized);
    let detect_select_into = supports_select_into_table_creation(database_type);
    let has_locking_clause = sql_contains_locking_clause(sql, parser_dialect.as_ref());
    // PostgreSQL-family and SQL Server SELECT INTO forms are represented in
    // the AST. Their text fallback also matches ordinary INSERT INTO, so only
    // use it for dialect-specific writes that the selected AST cannot express
    // (notably MySQL INTO OUTFILE/DUMPFILE and executable comments).
    let has_unparsed_dialect_specific_write =
        !detect_select_into && crate::query_execution_sql::has_dialect_specific_write(sql, database_type);

    match Parser::parse_sql(parser_dialect.as_ref(), sql) {
        Ok(statements) if !statements.is_empty() => {
            has_locking_clause
                || has_unparsed_dialect_specific_write
                || statements.iter().any(|statement| statement_is_dangerous(statement, detect_select_into))
        }
        _ => {
            has_locking_clause
                || crate::query_execution_sql::is_write_sql_for_database(sql, database_type)
                || sql_tokens_call_side_effect_function(sql, parser_dialect.as_ref())
        }
    }
}

/// MCP requests must select their database through the explicit request scope.
/// A `USE` statement mutates pooled/session state and could redirect later SQL,
/// so it is forbidden independently of read/write and high-risk permissions.
pub fn mcp_sql_has_forbidden_database_switch(sql: &str, database_type: DatabaseType) -> bool {
    if crate::query_execution_sql::classify_search_engine_query_risk(sql, database_type).is_some() {
        return false;
    }
    let database_type_name = format!("{database_type:?}");
    let normalized = normalize_dialect(&database_type_name);
    let dialect = resolve_dialect(normalized);
    if sql_has_use_statement(sql, dialect.as_ref()) {
        return true;
    }

    if matches!(
        database_type,
        DatabaseType::Mysql
            | DatabaseType::Doris
            | DatabaseType::StarRocks
            | DatabaseType::ManticoreSearch
            | DatabaseType::Goldendb
    ) {
        let executable_comments_expanded = crate::query_execution_sql::strip_sql_comments(sql);
        return sql_has_use_statement(&executable_comments_expanded, dialect.as_ref());
    }
    false
}

fn sql_has_use_statement(sql: &str, dialect: &dyn sqlparser::dialect::Dialect) -> bool {
    if let Ok(statements) = Parser::parse_sql(dialect, sql) {
        return statements.iter().any(|statement| matches!(statement, Statement::Use(_)));
    }

    let Ok(tokens) = Tokenizer::new(dialect, sql).tokenize() else {
        return false;
    };
    let mut statement_start = true;
    for token in tokens {
        match token {
            Token::Whitespace(_) => {}
            Token::SemiColon => statement_start = true,
            Token::Word(word) if statement_start => {
                if word.value.eq_ignore_ascii_case("use") {
                    return true;
                }
                statement_start = false;
            }
            Token::EOF => {}
            _ if statement_start => statement_start = false,
            _ => {}
        }
    }
    false
}

fn supports_select_into_table_creation(database_type: DatabaseType) -> bool {
    matches!(
        database_type,
        DatabaseType::Postgres
            | DatabaseType::Redshift
            | DatabaseType::Gaussdb
            | DatabaseType::OpenGauss
            | DatabaseType::Kingbase
            | DatabaseType::Highgo
            | DatabaseType::Uxdb
            | DatabaseType::Vastbase
            | DatabaseType::Kwdb
            | DatabaseType::SqlServer
    )
}

fn classify_sql_risk_with_database(
    sql: &str,
    normalized_dialect: &str,
    database_type: Option<DatabaseType>,
) -> Result<SqlRisk, String> {
    let parser_dialect = resolve_dialect(normalized_dialect);
    let detect_select_into = database_type.is_none();
    let has_locking_clause = sql_contains_locking_clause(sql, parser_dialect.as_ref());
    let has_dialect_specific_write = match database_type {
        Some(database_type) => crate::query_execution_sql::has_dialect_specific_write(sql, database_type),
        // Preserve MySQL executable-comment and file-output detection even
        // when callers provide a dialect string instead of a database type.
        None if normalized_dialect == "mysql" => {
            crate::query_execution_sql::has_dialect_specific_write(sql, DatabaseType::Mysql)
        }
        None => false,
    };

    match Parser::parse_sql(parser_dialect.as_ref(), sql) {
        Ok(stmts) if !stmts.is_empty() => {
            let mut max_risk = SqlRisk::ReadOnly;
            for stmt in &stmts {
                let risk = classify_statement(stmt, detect_select_into);
                if risk as u8 > max_risk as u8 {
                    max_risk = risk;
                }
            }
            if max_risk == SqlRisk::ReadOnly && (has_dialect_specific_write || has_locking_clause) {
                Ok(SqlRisk::Write)
            } else {
                Ok(max_risk)
            }
        }
        _ => {
            // Fallback: keyword-based classification
            let is_write = database_type.map_or_else(
                || crate::query_execution_sql::is_write_sql(sql),
                |database_type| crate::query_execution_sql::is_write_sql_for_database(sql, database_type),
            );
            // The keyword classifier cannot see calls such as `SELECT pg_terminate_backend(1)`,
            // so the side-effect denylist is applied at token level as well.
            if is_write || has_locking_clause || sql_tokens_call_side_effect_function(sql, parser_dialect.as_ref()) {
                Ok(SqlRisk::Write)
            } else {
                Ok(SqlRisk::ReadOnly)
            }
        }
    }
}

fn sql_contains_locking_clause(sql: &str, dialect: &dyn sqlparser::dialect::Dialect) -> bool {
    let Ok(tokens) = Tokenizer::new(dialect, sql).tokenize() else {
        return false;
    };
    let mut words: Vec<String> = Vec::with_capacity(4);
    for token in tokens {
        match token {
            Token::Word(word) if word.quote_style.is_none() => {
                words.push(word.value.to_ascii_uppercase());
                if words.len() > 4 {
                    words.remove(0);
                }
                if words.windows(2).any(|window| {
                    matches!(window, [r#for, lock] if r#for == "FOR" && matches!(lock.as_str(), "UPDATE" | "SHARE"))
                }) || words.windows(3).any(|window| {
                    matches!(window, [r#for, key, share] if r#for == "FOR" && key == "KEY" && share == "SHARE")
                }) || words.windows(4).any(|window| {
                    matches!(window, [r#for, no, key, update] if r#for == "FOR" && no == "NO" && key == "KEY" && update == "UPDATE")
                }) || words.windows(4).any(|window| {
                    matches!(window, [lock, r#in, share, mode] if lock == "LOCK" && r#in == "IN" && share == "SHARE" && mode == "MODE")
                }) {
                    return true;
                }
            }
            Token::Whitespace(_) => {}
            _ => words.clear(),
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Strict manual-transaction read-only proof (#9018)
//
// Unlike `classify_sql_risk*` (keyword fallback on parse failure may answer
// ReadOnly), the proof below is fail-closed end to end: the parse must succeed
// and yield exactly one statement, no write-capable construct may appear, and
// every called function must be an allowlisted pure builtin. The outcome only
// drives commit/rollback button visibility in manual transaction mode — it is
// a heuristic, never a security boundary (PG custom operators/casts and MySQL
// UDFs can hide arbitrary side effects static syntax cannot see).
// ---------------------------------------------------------------------------

/// Strict manual-transaction UX proof outcome. UI-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadProof {
    /// DBX's strict heuristic is satisfied for this statement.
    ProvenReadOnly,
    /// Anything unprovable. Buttons stay visible; execution is unaffected.
    Unproven,
}

/// Pure built-in functions accepted by the MySQL proof. Fail-closed: a call to
/// anything not listed (UDFs, session-state setters like `LAST_INSERT_ID(expr)`,
/// `SLEEP`/`GET_LOCK`, ...) is Unproven. Only ever grows by review.
const MYSQL_PROOF_SAFE_FUNCTIONS: &[&str] = &[
    "abs",
    "ascii",
    "avg",
    "bin",
    "ceiling",
    "char_length",
    "character_length",
    "coalesce",
    "concat",
    "concat_ws",
    "conv",
    "count",
    "crc32",
    "curdate",
    "curtime",
    "current_date",
    "current_time",
    "current_timestamp",
    "date_format",
    "datediff",
    "dayname",
    "dayofmonth",
    "dayofweek",
    "dayofyear",
    "exp",
    "floor",
    "format",
    "greatest",
    "hex",
    "hour",
    "if",
    "ifnull",
    "inet_aton",
    "inet_ntoa",
    "instr",
    "isnull",
    "json_extract",
    "json_length",
    "json_unquote",
    "json_valid",
    "last_day",
    "lcase",
    "least",
    "left",
    "length",
    "ln",
    "locate",
    "log",
    "log10",
    "log2",
    "lower",
    "lpad",
    "ltrim",
    "max",
    "md5",
    "microsecond",
    "min",
    "minute",
    "mod",
    "month",
    "monthname",
    "now",
    "nullif",
    "oct",
    "ord",
    "position",
    "pow",
    "power",
    "quarter",
    "rand",
    "repeat",
    "replace",
    "reverse",
    "right",
    "round",
    "rpad",
    "rtrim",
    "second",
    "sha",
    "sha1",
    "sha2",
    "sign",
    "space",
    "sqrt",
    "str_to_date",
    "substring",
    "substr",
    "time_format",
    "timediff",
    "timestampadd",
    "timestampdiff",
    "truncate",
    "unhex",
    "unix_timestamp",
    "upper",
    "ucase",
    "utc_date",
    "utc_time",
    "utc_timestamp",
    "uuid",
    "week",
    "weekday",
    "year",
];

/// Pure built-in functions accepted by the PostgreSQL proof. Same contract as
/// `MYSQL_PROOF_SAFE_FUNCTIONS`; sequence mutators/readers (`nextval`/`setval`),
/// `pg_sleep`, advisory-lock and large-object functions are deliberately absent.
const POSTGRES_PROOF_SAFE_FUNCTIONS: &[&str] = &[
    "abs",
    "age",
    "array_agg",
    "array_length",
    "ascii",
    "avg",
    "btrim",
    "cardinality",
    "ceil",
    "ceiling",
    "char_length",
    "character_length",
    "chr",
    "coalesce",
    "concat",
    "concat_ws",
    "count",
    "current_catalog",
    "current_date",
    "current_schema",
    "current_setting",
    "current_time",
    "current_timestamp",
    "current_user",
    "date_part",
    "date_trunc",
    "decode",
    "div",
    "encode",
    "every",
    "exp",
    "floor",
    "format",
    "gen_random_uuid",
    "greatest",
    "left",
    "length",
    "localtime",
    "localtimestamp",
    "log",
    "lower",
    "lpad",
    "ltrim",
    "max",
    "md5",
    "min",
    "mod",
    "now",
    "nullif",
    "position",
    "power",
    "repeat",
    "replace",
    "reverse",
    "right",
    "round",
    "rpad",
    "rtrim",
    "sha224",
    "sha256",
    "sha384",
    "sha512",
    "sign",
    "split_part",
    "sqrt",
    "starts_with",
    "strpos",
    "string_agg",
    "substr",
    "substring",
    "to_char",
    "to_date",
    "to_hex",
    "to_number",
    "to_timestamp",
    "trunc",
    "unnest",
    "upper",
    "version",
];

/// Prove that a single SQL statement is, by DBX's strict heuristic, an
/// ordinary read. Only MySQL/PostgreSQL take part in this proof; every other
/// database type is Unproven so its manual-transaction toolbar keeps the
/// legacy behavior. Oracle keeps its own lexical classifier
/// (`is_oracle_proven_read_only_statement`) — do not reroute it here.
pub fn prove_read_only_for_database(sql: &str, database_type: DatabaseType) -> ReadProof {
    match database_type {
        DatabaseType::Mysql => prove_read_only_statement(sql, "mysql", MYSQL_PROOF_SAFE_FUNCTIONS),
        DatabaseType::Postgres => prove_read_only_statement(sql, "postgres", POSTGRES_PROOF_SAFE_FUNCTIONS),
        _ => ReadProof::Unproven,
    }
}

fn prove_read_only_statement(sql: &str, dialect: &str, allowed_functions: &[&str]) -> ReadProof {
    let database_type = if dialect == "mysql" { DatabaseType::Mysql } else { DatabaseType::Postgres };
    // Lexical rejections that must not depend on parser support: dialect-specific
    // write syntax (executable comments, INTO OUTFILE/DUMPFILE, PostgreSQL
    // SELECT INTO) and MySQL session writes the parser models inconsistently
    // (SELECT @a := 1, SELECT ... INTO @var — INTO may sit in several positions).
    // `supports_select_into_table_creation` is deliberately NOT reused here: it
    // answers "SELECT INTO table creation" risk and excludes MySQL on purpose,
    // while the proof must reject any INTO target as session state.
    let cleaned = crate::query_execution_sql::strip_sql_comments_and_literals(sql);
    if cleaned.trim().is_empty() || crate::query_execution_sql::has_dialect_specific_write(sql, database_type) {
        return ReadProof::Unproven;
    }
    let parser_dialect = resolve_dialect(dialect);
    if dialect == "mysql"
        && (cleaned.contains(":=")
            || crate::query_execution_sql::contains_unquoted_keyword(&cleaned, parser_dialect.as_ref(), "INTO"))
    {
        return ReadProof::Unproven;
    }
    // The strict proof has NO keyword fallback: parse failure is Unproven, and
    // the API stays fail-closed even for callers that skipped pre-splitting.
    let Ok(statements) = Parser::parse_sql(parser_dialect.as_ref(), sql) else {
        return ReadProof::Unproven;
    };
    let [statement] = statements.as_slice() else {
        return ReadProof::Unproven;
    };
    prove_statement(statement, allowed_functions)
}

fn prove_statement(statement: &Statement, allowed_functions: &[&str]) -> ReadProof {
    match statement {
        Statement::Query(query) => {
            // Reuses the risk engine's structural walk: writable CTEs, DML set
            // branches, locking clauses, SELECT INTO and denylisted side-effect
            // functions. detect_select_into is always true — a proof is about a
            // clean session, not table-creation risk.
            if query_is_write_capable(query, true) {
                return ReadProof::Unproven;
            }
            let mut visitor = ProofFunctionVisitor { allowed_functions, rejected: false };
            let _ = query.visit(&mut visitor);
            if visitor.rejected {
                ReadProof::Unproven
            } else {
                ReadProof::ProvenReadOnly
            }
        }
        // Plain EXPLAIN never executes the statement; EXPLAIN ANALYZE does.
        Statement::Explain { analyze: false, statement, .. } => prove_statement(statement, allowed_functions),
        Statement::Explain { .. } => ReadProof::Unproven,
        // MySQL `DESC t` / `DESCRIBE t`.
        Statement::ExplainTable { .. } => ReadProof::ProvenReadOnly,
        statement if is_show_read_only_statement(statement) => ReadProof::ProvenReadOnly,
        _ => ReadProof::Unproven,
    }
}

/// Full-statement allowlist scan. Deliberately uses the `visitor`-feature walk
/// (covers CTE bodies, FROM subqueries, LATERAL derived tables and ORDER BY
/// expressions) instead of `visit_expressions`, which skips FROM subqueries.
/// Window functions and table functions (`FROM generate_series(...)`) are
/// rejected outright in v1 — fail-closed, relaxed only by review.
struct ProofFunctionVisitor<'a> {
    allowed_functions: &'a [&'a str],
    rejected: bool,
}

impl Visitor for ProofFunctionVisitor<'_> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if let Expr::Function(function) = expr {
            if function.over.is_some() {
                self.rejected = true;
                return ControlFlow::Break(());
            }
            let parts = &function.name.0;
            let allowed = parts.len() == 1
                && parts.last().and_then(|part| part.as_ident()).is_some_and(|ident| {
                    self.allowed_functions.iter().any(|candidate| ident.value.eq_ignore_ascii_case(candidate))
                });
            if !allowed {
                self.rejected = true;
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<()> {
        if let TableFactor::Table { args: Some(_), .. } = table_factor {
            self.rejected = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }
}

// ---------------------------------------------------------------------------
// Strict read-only function check for security-sensitive read-only contexts
//
// `classify_sql_risk*` only rejects *known* side-effect functions. In the strict contexts (MCP
// read-only policy, read-only connections, AI auto-execution without a confirmation) a read
// must additionally not call anything DBX cannot vouch for: on MySQL- and PostgreSQL-family
// databases user-defined functions can write, so any function outside a reviewed allowlist of
// pure built-ins makes the statement unproven. Other engines only get the denylist, because
// their functions cannot write from a SELECT (SQL Server, SQLite) or their function vocabulary
// is not covered by an allowlist yet.
// ---------------------------------------------------------------------------

/// Pure built-ins shared by the MySQL and PostgreSQL strict allowlists (aggregates, window
/// functions and common scalar functions).
const COMMON_STRICT_SAFE_FUNCTIONS: &[&str] = &[
    "acos",
    "array_agg",
    "asin",
    "atan",
    "atan2",
    "bit_and",
    "bit_length",
    "bit_or",
    "cast",
    "ceil",
    "char",
    "cos",
    "cot",
    "cume_dist",
    "current_role",
    "current_user",
    "decode",
    "degrees",
    "dense_rank",
    "extract",
    "first_value",
    "json_array",
    "json_arrayagg",
    "json_object",
    "json_objectagg",
    "json_value",
    "lag",
    "last_value",
    "lead",
    "localtime",
    "localtimestamp",
    "nth_value",
    "ntile",
    "nvl",
    "nvl2",
    "octet_length",
    "percent_rank",
    "pi",
    "radians",
    "random",
    "rank",
    "regexp_instr",
    "regexp_like",
    "regexp_replace",
    "regexp_substr",
    "row_number",
    "session_user",
    "sin",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "sum",
    "sysdate",
    "system_user",
    "tan",
    "trim",
    "user",
    "var_pop",
    "var_samp",
    "variance",
];

/// Extra MySQL-family pure built-ins for the strict check (on top of the manual-transaction
/// proof list, which stays deliberately small).
const MYSQL_STRICT_SAFE_FUNCTIONS: &[&str] = &[
    "adddate",
    "addtime",
    "any_value",
    "bin_to_uuid",
    "bit_count",
    "bit_xor",
    "charset",
    "coercibility",
    "collation",
    "connection_id",
    "convert",
    "convert_tz",
    "database",
    "date",
    "date_add",
    "date_sub",
    "day",
    "elt",
    "export_set",
    "field",
    "find_in_set",
    "found_rows",
    "from_base64",
    "from_days",
    "from_unixtime",
    "get_format",
    "group_concat",
    "inet6_aton",
    "inet6_ntoa",
    "insert",
    "is_ipv4",
    "is_ipv6",
    "is_uuid",
    "json_array_append",
    "json_array_insert",
    "json_contains",
    "json_contains_path",
    "json_depth",
    "json_insert",
    "json_keys",
    "json_merge_patch",
    "json_merge_preserve",
    "json_overlaps",
    "json_pretty",
    "json_quote",
    "json_remove",
    "json_replace",
    "json_schema_valid",
    "json_search",
    "json_set",
    "json_storage_size",
    "json_table",
    "json_type",
    "makedate",
    "maketime",
    "mid",
    "period_add",
    "period_diff",
    "quote",
    "row_count",
    "schema",
    "sec_to_time",
    "std",
    "strcmp",
    "subdate",
    "substring_index",
    "subtime",
    "tidb_decode_key",
    "tidb_decode_plan",
    "tidb_is_ddl_owner",
    "tidb_parse_tso",
    "tidb_version",
    "time",
    "time_to_sec",
    "timestamp",
    "to_base64",
    "to_days",
    "to_seconds",
    "uuid_to_bin",
    "version",
    "weekofyear",
    "yearweek",
];

/// Extra PostgreSQL-family pure built-ins (including common catalog/introspection helpers and
/// set-returning functions used in `FROM`) for the strict check.
const POSTGRES_STRICT_SAFE_FUNCTIONS: &[&str] = &[
    "array_append",
    "array_cat",
    "array_dims",
    "array_fill",
    "array_lower",
    "array_ndims",
    "array_position",
    "array_positions",
    "array_prepend",
    "array_remove",
    "array_replace",
    "array_to_json",
    "array_to_string",
    "array_upper",
    "bool_and",
    "bool_or",
    "cbrt",
    "clock_timestamp",
    "col_description",
    "convert_from",
    "convert_to",
    "corr",
    "covar_pop",
    "covar_samp",
    "current_database",
    "date_add",
    "date_bin",
    "date_subtract",
    "factorial",
    "format_type",
    "gcd",
    "generate_series",
    "generate_subscripts",
    "has_column_privilege",
    "has_database_privilege",
    "has_function_privilege",
    "has_schema_privilege",
    "has_table_privilege",
    "inet_client_addr",
    "inet_server_addr",
    "initcap",
    "isfinite",
    "json_agg",
    "json_array_elements",
    "json_array_elements_text",
    "json_array_length",
    "json_build_array",
    "json_build_object",
    "json_each",
    "json_each_text",
    "json_extract_path",
    "json_extract_path_text",
    "json_object_agg",
    "json_object_keys",
    "json_populate_record",
    "json_populate_recordset",
    "json_strip_nulls",
    "json_to_record",
    "json_to_recordset",
    "json_typeof",
    "jsonb_agg",
    "jsonb_array_elements",
    "jsonb_array_elements_text",
    "jsonb_array_length",
    "jsonb_build_array",
    "jsonb_build_object",
    "jsonb_each",
    "jsonb_each_text",
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "jsonb_insert",
    "jsonb_object_agg",
    "jsonb_object_keys",
    "jsonb_path_exists",
    "jsonb_path_match",
    "jsonb_path_query",
    "jsonb_path_query_array",
    "jsonb_path_query_first",
    "jsonb_populate_record",
    "jsonb_populate_recordset",
    "jsonb_pretty",
    "jsonb_set",
    "jsonb_strip_nulls",
    "jsonb_to_record",
    "jsonb_to_recordset",
    "jsonb_typeof",
    "justify_days",
    "justify_hours",
    "justify_interval",
    "lcm",
    "ln",
    "log10",
    "make_date",
    "make_interval",
    "make_time",
    "make_timestamp",
    "make_timestamptz",
    "mode",
    "obj_description",
    "overlay",
    "percentile_cont",
    "percentile_disc",
    "pg_backend_pid",
    "pg_database_size",
    "pg_get_constraintdef",
    "pg_get_expr",
    "pg_get_functiondef",
    "pg_get_indexdef",
    "pg_get_serial_sequence",
    "pg_get_triggerdef",
    "pg_get_userbyid",
    "pg_get_viewdef",
    "pg_indexes_size",
    "pg_is_in_recovery",
    "pg_postmaster_start_time",
    "pg_relation_size",
    "pg_size_pretty",
    "pg_table_is_visible",
    "pg_table_size",
    "pg_total_relation_size",
    "pg_typeof",
    "quote_ident",
    "quote_literal",
    "quote_nullable",
    "regexp_count",
    "regexp_match",
    "regexp_matches",
    "regexp_split_to_array",
    "regexp_split_to_table",
    "regr_slope",
    "row_to_json",
    "scale",
    "statement_timestamp",
    "string_to_array",
    "timeofday",
    "to_ascii",
    "to_json",
    "to_jsonb",
    "to_regclass",
    "to_regtype",
    "transaction_timestamp",
    "translate",
    "trim_scale",
    "width_bucket",
];

const MYSQL_STRICT_ALLOWLISTS: &[&[&str]] =
    &[MYSQL_PROOF_SAFE_FUNCTIONS, MYSQL_STRICT_SAFE_FUNCTIONS, COMMON_STRICT_SAFE_FUNCTIONS];
const POSTGRES_STRICT_ALLOWLISTS: &[&[&str]] =
    &[POSTGRES_PROOF_SAFE_FUNCTIONS, POSTGRES_STRICT_SAFE_FUNCTIONS, COMMON_STRICT_SAFE_FUNCTIONS];

/// Words that may precede `(` without being a function call (clauses, operators, type names in
/// casts). Only consulted by the token-level fallback for SQL the parser rejects.
const NON_FUNCTION_WORDS_BEFORE_PAREN: &[&str] = &[
    "all",
    "and",
    "any",
    "array",
    "as",
    "between",
    "bigint",
    "binary",
    "bit",
    "by",
    "case",
    "char",
    "character",
    "cube",
    "date",
    "datetime",
    "dec",
    "decimal",
    "distinct",
    "double",
    "else",
    "enum",
    "except",
    "exists",
    "filter",
    "float",
    "from",
    "group",
    "grouping",
    "groups",
    "having",
    "in",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "join",
    "json",
    "lateral",
    "like",
    "limit",
    "mediumint",
    "nchar",
    "not",
    "numeric",
    "nvarchar",
    "offset",
    "on",
    "or",
    "order",
    "over",
    "partition",
    "precision",
    "range",
    "real",
    "rollup",
    "row",
    "rows",
    "select",
    "set",
    "sets",
    "signed",
    "smallint",
    "some",
    "table",
    "then",
    "time",
    "timestamp",
    "tinyint",
    "union",
    "unsigned",
    "using",
    "values",
    "varbinary",
    "varchar",
    "when",
    "where",
    "window",
    "with",
    "within",
    "year",
];

fn strict_function_allowlists(database_type: DatabaseType) -> Option<&'static [&'static [&'static str]]> {
    match database_type {
        DatabaseType::Mysql | DatabaseType::Goldendb => Some(MYSQL_STRICT_ALLOWLISTS),
        DatabaseType::Postgres
        | DatabaseType::Gaussdb
        | DatabaseType::OpenGauss
        | DatabaseType::Kingbase
        | DatabaseType::Highgo
        | DatabaseType::Uxdb
        | DatabaseType::Vastbase
        | DatabaseType::Kwdb => Some(POSTGRES_STRICT_ALLOWLISTS),
        _ => None,
    }
}

/// Engines whose query text is not SQL; the function scan does not apply to them.
fn is_non_sql_query_database(database_type: DatabaseType) -> bool {
    matches!(
        database_type,
        DatabaseType::Redis
            | DatabaseType::MongoDb
            | DatabaseType::DynamoDb
            | DatabaseType::Elasticsearch
            | DatabaseType::Easysearch
            | DatabaseType::Solr
            | DatabaseType::Meilisearch
            | DatabaseType::VictoriaMetrics
            | DatabaseType::Qdrant
            | DatabaseType::Milvus
            | DatabaseType::Weaviate
            | DatabaseType::ChromaDb
            | DatabaseType::Etcd
            | DatabaseType::Consul
            | DatabaseType::ZooKeeper
            | DatabaseType::Nacos
            | DatabaseType::Mqtt
            | DatabaseType::MessageQueue
            | DatabaseType::Neo4j
    )
}

fn strict_function_violation(name_parts: &[String], allowlists: Option<&[&[&str]]>) -> Option<String> {
    let display = name_parts.join(".");
    if is_side_effect_table_function(name_parts) {
        return Some(format!(
            "It calls {display}(), which can change database or server state or reach outside the database."
        ));
    }
    let allowlists = allowlists?;
    let name = name_parts.last()?.to_ascii_lowercase();
    let qualifier_allowed = match name_parts {
        [_] => true,
        [schema, _] => schema.eq_ignore_ascii_case("pg_catalog"),
        _ => false,
    };
    let allowed = qualifier_allowed && allowlists.iter().any(|list| list.contains(&name.as_str()));
    (!allowed).then(|| {
        format!(
            "It calls {display}(), which is not on DBX's list of known read-only built-in functions, \
             so DBX cannot prove the statement has no side effects."
        )
    })
}

struct StrictFunctionVisitor {
    allowlists: Option<&'static [&'static [&'static str]]>,
    violation: Option<String>,
}

impl Visitor for StrictFunctionVisitor {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if let Expr::Function(function) = expr {
            self.violation = strict_function_violation(&object_name_parts(&function.name), self.allowlists);
            if self.violation.is_some() {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<()> {
        if let Some(name) = table_factor_function_name(table_factor) {
            self.violation = strict_function_violation(&object_name_parts(name), self.allowlists);
            if self.violation.is_some() {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }
}

/// Strict read-only function check for security-sensitive contexts: MCP read-only execution,
/// connections with read-only protection, and AI-agent SQL that runs without a user
/// confirmation. Returns the reason when a statement that is otherwise classified as a read
/// cannot be proven free of side effects:
///
/// * any call of a known side-effecting function (all SQL engines, also when the statement
///   does not parse);
/// * on MySQL- and PostgreSQL-family engines, any function outside the reviewed allowlist of pure
///   built-ins (user-defined functions can write there), and MySQL session-variable assignment
///   (`:=`, `INTO @var`).
///
/// Callers must still reject writes separately (`classify_sql_risk_for_database`,
/// `is_write_sql_for_database`); this only narrows what counts as a read.
pub fn strict_read_only_violation_for_database(sql: &str, database_type: DatabaseType) -> Option<String> {
    if is_non_sql_query_database(database_type)
        || crate::query_execution_sql::classify_search_engine_query_risk(sql, database_type).is_some()
    {
        return None;
    }
    let allowlists = strict_function_allowlists(database_type);
    let database_type_name = format!("{database_type:?}");
    let parser_dialect = resolve_dialect(normalize_dialect(&database_type_name));

    if matches!(database_type, DatabaseType::Mysql | DatabaseType::Goldendb) {
        let cleaned = crate::query_execution_sql::strip_sql_comments_and_literals(sql);
        if cleaned.contains(":=")
            || crate::query_execution_sql::contains_unquoted_keyword(&cleaned, parser_dialect.as_ref(), "INTO")
        {
            return Some("It assigns MySQL session variables (`:=` or `INTO`).".to_string());
        }
    }

    match Parser::parse_sql(parser_dialect.as_ref(), sql) {
        Ok(statements) => {
            let mut visitor = StrictFunctionVisitor { allowlists, violation: None };
            for statement in &statements {
                let _ = statement.visit(&mut visitor);
                if visitor.violation.is_some() {
                    break;
                }
            }
            visitor.violation
        }
        Err(_) => {
            let Some(calls) = function_calls_in_tokens(sql, parser_dialect.as_ref()) else {
                return allowlists
                    .map(|_| "DBX could not analyse the statement, so it cannot prove it is read-only.".to_string());
            };
            calls.iter().find_map(|call| {
                let is_structural_word = call.parts.len() == 1
                    && !call.quoted
                    && NON_FUNCTION_WORDS_BEFORE_PAREN.contains(&call.parts[0].to_ascii_lowercase().as_str());
                if is_structural_word {
                    None
                } else {
                    strict_function_violation(&call.parts, allowlists)
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_proof_accepts_plain_reads_and_allowlisted_functions() {
        for sql in [
            "SELECT * FROM users",
            "SELECT COUNT(*) FROM orders",
            "SELECT id, name FROM users WHERE id IN (1, 2, 3)",
            "SELECT id, name FROM users WHERE id IN (SELECT user_id FROM roles) ORDER BY created_at LIMIT 10",
            "SELECT 'update' FROM t",
            "SELECT a FROM t UNION ALL SELECT b FROM u",
            "WITH totals AS (SELECT COUNT(*) AS n FROM orders) SELECT n FROM totals",
            "SELECT CONCAT(first_name, ' ', last_name) AS full_name FROM users",
            "SELECT id FROM users ORDER BY rand() LIMIT 1",
            "SHOW TABLES",
            "SHOW CREATE TABLE users",
            "EXPLAIN SELECT * FROM users",
            "DESC users",
            "DESCRIBE users",
            "-- only a comment header\nSELECT id FROM users",
            "SELECT 1",
        ] {
            assert_eq!(
                prove_read_only_for_database(sql, DatabaseType::Mysql),
                ReadProof::ProvenReadOnly,
                "expected proven: {sql}"
            );
        }
    }

    #[test]
    fn mysql_proof_rejects_session_writes_locking_and_unknown_functions() {
        for sql in [
            "SELECT @a := 1",
            "SELECT 1 INTO @current_id",
            "SELECT id FROM users INTO @current_id LIMIT 1",
            "SELECT * FROM users INTO OUTFILE '/tmp/x'",
            "SELECT * FROM users FOR UPDATE",
            "SELECT * FROM users FOR SHARE",
            "SELECT * FROM users LOCK IN SHARE MODE",
            "SELECT SLEEP(10)",
            "SELECT GET_LOCK('x', 1)",
            "SELECT LAST_INSERT_ID()",
            "SELECT my_custom_udf(1)",
            "SELECT secret_schema.fn(1) FROM t",
            "SELECT id FROM users WHERE EXISTS (SELECT SLEEP(1))",
            "WITH x AS (SELECT SLEEP(1)) SELECT 1",
            "SELECT ROW_NUMBER() OVER (ORDER BY id) FROM users",
            "SELECT id FROM users; SELECT 1",
            "EXPLAIN ANALYZE SELECT * FROM users",
            "SELEC * FORM users",
            "SET autocommit = 1",
        ] {
            assert_eq!(
                prove_read_only_for_database(sql, DatabaseType::Mysql),
                ReadProof::Unproven,
                "expected unproven: {sql}"
            );
        }
    }

    #[test]
    fn postgres_proof_accepts_plain_reads_and_rejects_side_effects() {
        for sql in [
            "SELECT * FROM users",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SELECT * FROM t WHERE id IN (SELECT id FROM u)",
            "SELECT coalesce(a, b) FROM t",
            "SELECT split_part(email, '@', 2) FROM users",
            "EXPLAIN SELECT 1",
            "SELECT 1",
        ] {
            assert_eq!(
                prove_read_only_for_database(sql, DatabaseType::Postgres),
                ReadProof::ProvenReadOnly,
                "expected proven: {sql}"
            );
        }
        for sql in [
            "WITH w AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM w",
            "SELECT * INTO new_t FROM t",
            "SELECT * FROM t FOR UPDATE",
            // FOR SHARE parses into query.locks like FOR UPDATE; the other two
            // PG lock strengths (FOR NO KEY UPDATE / FOR KEY SHARE) fail the
            // parse itself in sqlparser 0.62 and stay Unproven fail-closed.
            "SELECT * FROM t FOR SHARE",
            "SELECT * FROM t FOR NO KEY UPDATE",
            "SELECT * FROM t FOR KEY SHARE",
            "SELECT nextval('seq')",
            "SELECT setval('seq', 1)",
            "SELECT pg_sleep(1)",
            "SELECT lo_import('/etc/passwd')",
            "EXPLAIN ANALYZE SELECT 1",
            "SELECT my_udf()",
            "SELECT id, row_number() OVER () FROM t",
            "SELECT 1; SELECT 2",
        ] {
            assert_eq!(
                prove_read_only_for_database(sql, DatabaseType::Postgres),
                ReadProof::Unproven,
                "expected unproven: {sql}"
            );
        }
    }

    #[test]
    fn proof_visitor_covers_from_subqueries_ctes_lateral_and_order_by() {
        // visit_expressions skips FROM subqueries; the proof's visitor must not.
        for sql in [
            "SELECT * FROM (SELECT SLEEP(1)) AS x",
            "SELECT * FROM t ORDER BY SLEEP(1)",
            "SELECT a FROM t, LATERAL (SELECT my_udf() FROM u) d",
            "SELECT * FROM generate_series(1, 10)",
        ] {
            assert_eq!(
                prove_read_only_for_database(sql, DatabaseType::Postgres),
                ReadProof::Unproven,
                "expected unproven: {sql}"
            );
        }
        assert_eq!(
            prove_read_only_for_database("SELECT * FROM (SELECT 1) AS x ORDER BY md5(id)", DatabaseType::Postgres),
            ReadProof::ProvenReadOnly
        );
    }

    #[test]
    fn proof_only_applies_to_enabled_dialects() {
        // Oracle keeps its own lexical classifier; the gate in query.rs must
        // never route it through the generic proof.
        assert_eq!(prove_read_only_for_database("SELECT 1", DatabaseType::Oracle), ReadProof::Unproven);
        // Family members without manual-transaction UI stay unproven in v1.
        assert_eq!(prove_read_only_for_database("SELECT 1", DatabaseType::Doris), ReadProof::Unproven);
    }

    #[test]
    fn classify_select_statements() {
        assert_eq!(classify_sql_risk("SELECT * FROM users", "postgres").unwrap(), SqlRisk::ReadOnly);
        assert_eq!(
            classify_sql_risk("SELECT id, name FROM users WHERE active = true", "mysql").unwrap(),
            SqlRisk::ReadOnly
        );
        assert_eq!(classify_sql_risk("SHOW TABLES", "mysql").unwrap(), SqlRisk::ReadOnly);
        assert_eq!(classify_sql_risk("DESCRIBE users", "mysql").unwrap(), SqlRisk::ReadOnly);
        assert_eq!(classify_sql_risk("EXPLAIN SELECT * FROM users", "postgres").unwrap(), SqlRisk::ReadOnly);
    }

    #[test]
    fn classify_mysql_show_triggers_as_read_only() {
        for sql in [
            "SHOW TRIGGERS;",
            "SHOW TRIGGERS FROM `rs_main` LIKE 'trg_order_items_after_%';",
            "show triggers in `rs_main` where `Event` = 'INSERT';",
        ] {
            assert_eq!(classify_sql_risk(sql, "mysql").unwrap(), SqlRisk::ReadOnly, "expected read-only: {sql}");
            assert_eq!(
                classify_sql_risk_for_database(sql, DatabaseType::Mysql).unwrap(),
                SqlRisk::ReadOnly,
                "expected read-only: {sql}"
            );
            assert!(!is_dangerous_sql_for_database(sql, DatabaseType::Mysql), "expected safe SQL: {sql}");
        }
    }

    #[test]
    fn classify_supported_show_statements_as_read_only() {
        for (sql, database_type) in [
            ("SHOW COLLATION", DatabaseType::Mysql),
            ("SHOW CHARACTER SET", DatabaseType::Mysql),
            ("SHOW search_path", DatabaseType::Postgres),
        ] {
            assert_eq!(
                classify_sql_risk_for_database(sql, database_type).unwrap(),
                SqlRisk::ReadOnly,
                "expected read-only: {sql}"
            );
        }
    }

    #[test]
    fn mysql_show_triggers_preserves_write_detection() {
        for (sql, expected_risk) in [
            ("SHOW TRIGGERS; DELETE FROM order_items", SqlRisk::Write),
            ("SHOW TRIGGERS; DROP TABLE order_items", SqlRisk::Ddl),
            ("SHOW TRIGGERS; /*!50000 DELETE FROM order_items */", SqlRisk::Write),
        ] {
            assert_eq!(classify_sql_risk(sql, "mysql").unwrap(), expected_risk, "expected write detection: {sql}");
            assert_eq!(
                classify_sql_risk_for_database(sql, DatabaseType::Mysql).unwrap(),
                expected_risk,
                "expected write detection: {sql}"
            );
            assert!(is_dangerous_sql_for_database(sql, DatabaseType::Mysql), "expected dangerous SQL: {sql}");
        }
    }

    #[test]
    fn classify_mysql_legacy_shared_lock_reads_like_for_share() {
        // MySQL's pre-8.0.1 spelling of `FOR SHARE` is still valid in 8.0.x and
        // takes the same shared row locks, so it must not classify as a plain read.
        for sql in [
            "SELECT * FROM users LOCK IN SHARE MODE",
            "SELECT * FROM users lock in share mode",
            "SELECT id FROM users WHERE id = 1 LOCK IN SHARE MODE",
            "WITH c AS (SELECT 1) SELECT * FROM users LOCK IN SHARE MODE",
            "SELECT * FROM (SELECT * FROM users LOCK IN SHARE MODE) AS locked_users",
            "WITH locked AS (SELECT * FROM users LOCK IN SHARE MODE) SELECT * FROM locked",
            "SELECT (SELECT id FROM users LOCK IN SHARE MODE)",
        ] {
            assert_eq!(
                classify_sql_risk_for_database(sql, DatabaseType::Mysql).unwrap(),
                SqlRisk::Write,
                "expected locking read to be write-capable: {sql}"
            );
            assert!(is_dangerous_sql_for_database(sql, DatabaseType::Mysql), "expected high-risk SQL: {sql}");
        }

        // A column or alias literally named "mode" must stay a plain read.
        for sql in [
            "SELECT lock_in_share_mode FROM settings",
            "SELECT mode FROM share WHERE id = 1",
            "SELECT 'LOCK IN SHARE MODE'",
            "SELECT lock, `in`, share, mode FROM settings",
        ] {
            assert_eq!(
                classify_sql_risk_for_database(sql, DatabaseType::Mysql).unwrap(),
                SqlRisk::ReadOnly,
                "expected plain read: {sql}"
            );
        }
    }

    #[test]
    fn classify_cte_read() {
        assert_eq!(
            classify_sql_risk("WITH cte AS (SELECT 1) SELECT * FROM cte", "postgres").unwrap(),
            SqlRisk::ReadOnly
        );
    }

    #[test]
    fn classify_writable_ctes_recursively() {
        for sql in [
            "WITH inserted AS (INSERT INTO users (id) VALUES (1) RETURNING id) SELECT * FROM inserted",
            "WITH updated AS (UPDATE users SET active = true WHERE id = 1 RETURNING id) SELECT * FROM updated",
            "WITH deleted AS (DELETE FROM users WHERE id = 1 RETURNING id) SELECT * FROM deleted",
            "WITH merged AS (MERGE INTO users USING staged_users ON users.id = staged_users.id WHEN MATCHED THEN UPDATE SET active = true RETURNING users.id) SELECT * FROM merged",
            "WITH outer_cte AS (WITH deleted AS (DELETE FROM users WHERE id = 1 RETURNING id) SELECT * FROM deleted) SELECT * FROM outer_cte",
        ] {
            assert_eq!(classify_sql_risk(sql, "postgres").unwrap(), SqlRisk::Write, "expected writable CTE: {sql}");
            assert!(
                crate::query_execution_sql::is_write_sql_for_database(sql, DatabaseType::Postgres),
                "expected writable CTE to trip read-only enforcement: {sql}"
            );
        }
    }

    #[test]
    fn writable_cte_danger_tracks_the_nested_mutation() {
        for sql in [
            "WITH updated AS (UPDATE users SET active = true WHERE id = 1 RETURNING id) SELECT * FROM updated",
            "WITH deleted AS (DELETE FROM users WHERE id = 1 RETURNING id) SELECT * FROM deleted",
            "WITH outer_cte AS (WITH deleted AS (DELETE FROM users WHERE id = 1 RETURNING id) SELECT * FROM deleted) SELECT * FROM outer_cte",
        ] {
            assert!(!is_dangerous_sql_for_database(sql, DatabaseType::Postgres), "expected guarded write: {sql}");
        }

        for sql in [
            "WITH updated AS (UPDATE users SET active = true RETURNING id) SELECT * FROM updated",
            "WITH deleted AS (DELETE FROM users RETURNING id) SELECT * FROM deleted",
            "WITH outer_cte AS (WITH deleted AS (DELETE FROM users RETURNING id) SELECT * FROM deleted) SELECT * FROM outer_cte",
        ] {
            assert!(is_dangerous_sql_for_database(sql, DatabaseType::Postgres), "expected dangerous write: {sql}");
        }
    }

    #[test]
    fn classify_write_statements() {
        assert_eq!(classify_sql_risk("INSERT INTO users VALUES (1)", "postgres").unwrap(), SqlRisk::Write);
        assert_eq!(classify_sql_risk("UPDATE users SET name = 'x'", "postgres").unwrap(), SqlRisk::Write);
        assert_eq!(classify_sql_risk("DELETE FROM users", "postgres").unwrap(), SqlRisk::Write);
        assert_eq!(classify_sql_risk("EXPLAIN ANALYZE DELETE FROM users", "postgres").unwrap(), SqlRisk::Write);
        assert_eq!(classify_sql_risk("SELECT * INTO backup_users FROM users", "postgres").unwrap(), SqlRisk::Write);
        assert_eq!(
            classify_sql_risk("SELECT * FROM users INTO OUTFILE '/tmp/users.csv'", "mysql").unwrap(),
            SqlRisk::Write
        );
        assert_eq!(classify_sql_risk("/*! DELETE FROM users */", "mysql").unwrap(), SqlRisk::Write);
    }

    #[test]
    fn mcp_forbids_persistent_database_switching_in_every_permission_mode() {
        for (sql, database_type) in [
            ("USE reporting", DatabaseType::Mysql),
            ("-- target database\nUSE reporting", DatabaseType::Mysql),
            ("SELECT 1; USE reporting", DatabaseType::Mysql),
            ("USE [reporting]", DatabaseType::SqlServer),
            ("USE DATABASE reporting", DatabaseType::Snowflake),
            ("/*!50000 USE reporting */", DatabaseType::Mysql),
        ] {
            assert!(mcp_sql_has_forbidden_database_switch(sql, database_type), "expected blocked SQL: {sql}");
        }

        for sql in ["SELECT use FROM feature_flags", "SELECT 'USE reporting'", "SELECT 1"] {
            assert!(!mcp_sql_has_forbidden_database_switch(sql, DatabaseType::Mysql), "expected allowed SQL: {sql}");
        }
        assert!(!mcp_sql_has_forbidden_database_switch("/*!50000 USE reporting */", DatabaseType::Postgres));
    }

    #[test]
    fn classify_known_side_effect_selects_and_copy_as_writes() {
        for sql in [
            "SELECT setval('user_id_seq', 42)",
            "SELECT nextval('user_id_seq')",
            "SELECT pg_terminate_backend(42)",
            "SELECT * FROM users FOR UPDATE",
            "SELECT * FROM users FOR KEY SHARE",
            "COPY users TO '/tmp/users.csv'",
            "COPY (SELECT * FROM users) TO PROGRAM 'cat > /tmp/users.csv'",
        ] {
            assert_eq!(
                classify_sql_risk_for_database(sql, DatabaseType::Postgres).unwrap(),
                SqlRisk::Write,
                "expected write-capable SQL: {sql}"
            );
            assert!(is_dangerous_sql_for_database(sql, DatabaseType::Postgres), "expected high-risk SQL: {sql}");
        }
    }

    #[test]
    fn classify_backend_query_cancellation_as_a_side_effect() {
        for (sql, database_type) in [
            ("SELECT pg_cancel_backend(42)", DatabaseType::Postgres),
            ("SELECT sys_cancel_backend(42)", DatabaseType::Kingbase),
        ] {
            assert_eq!(
                classify_sql_risk_for_database(sql, database_type).unwrap(),
                SqlRisk::Write,
                "expected query cancellation to be write-capable: {sql}"
            );
            assert!(
                is_dangerous_sql_for_database(sql, database_type),
                "expected query cancellation to require production protection: {sql}"
            );
        }
    }

    #[test]
    fn classify_dialect_specific_select_into_as_write() {
        for sql in [
            "SELECT 3156 INTO OUTFILE '/var/lib/mysql-files/dbx_ro_probe.txt'",
            "SELECT 3156 INTO DUMPFILE '/var/lib/mysql-files/dbx_ro_probe.bin'",
        ] {
            assert_eq!(classify_sql_risk_for_database(sql, DatabaseType::Mysql).unwrap(), SqlRisk::Write);
        }

        for database_type in [
            DatabaseType::Postgres,
            DatabaseType::Redshift,
            DatabaseType::Gaussdb,
            DatabaseType::OpenGauss,
            DatabaseType::Kingbase,
            DatabaseType::Highgo,
            DatabaseType::Vastbase,
            DatabaseType::Kwdb,
        ] {
            assert_eq!(
                classify_sql_risk_for_database("SELECT * INTO copied_users FROM users", database_type).unwrap(),
                SqlRisk::Write,
                "expected PostgreSQL-family SELECT INTO to be a write for {database_type:?}"
            );
        }
        assert_eq!(
            classify_sql_risk_for_database("SELECT * INTO #copied_users FROM users", DatabaseType::SqlServer).unwrap(),
            SqlRisk::Write
        );
        assert_eq!(
            classify_sql_risk_for_database(
                "SELECT 3156 /*!50000 INTO OUTFILE '/var/lib/mysql-files/dbx_ro_probe.txt' */",
                DatabaseType::Mysql,
            )
            .unwrap(),
            SqlRisk::Write
        );
    }

    #[test]
    fn typed_classification_preserves_existing_risk_levels() {
        assert_eq!(
            classify_sql_risk_for_database("SELECT * FROM users", DatabaseType::Postgres).unwrap(),
            SqlRisk::ReadOnly
        );
        assert_eq!(
            classify_sql_risk_for_database("CREATE TABLE users (id INT)", DatabaseType::Postgres).unwrap(),
            SqlRisk::Ddl
        );
        assert_eq!(
            classify_sql_risk_for_database("SELECT 1 INTO unsupported", DatabaseType::Sqlite).unwrap(),
            SqlRisk::ReadOnly
        );
    }

    #[test]
    fn classify_ddl_statements() {
        assert_eq!(classify_sql_risk("CREATE TABLE users (id INT)", "postgres").unwrap(), SqlRisk::Ddl);
        assert_eq!(classify_sql_risk("DROP TABLE users", "postgres").unwrap(), SqlRisk::Ddl);
        assert_eq!(classify_sql_risk("ALTER TABLE users ADD COLUMN age INT", "postgres").unwrap(), SqlRisk::Ddl);
        assert_eq!(classify_sql_risk("TRUNCATE TABLE users", "postgres").unwrap(), SqlRisk::Ddl);
    }

    #[test]
    fn high_risk_sql_requires_central_permission_for_unbounded_changes() {
        assert!(is_dangerous_sql_for_database("TRUNCATE TABLE users", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("DELETE FROM users", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("UPDATE users SET active = 0", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE 1 = 1", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("UPDATE users SET active = 0 WHERE TRUE", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE id = id", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE lower(email) = lower(email)",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE NOT (1 = 0)", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("UPDATE users SET active = 0 WHERE 2 > 1", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE id IS NULL OR id IS NOT NULL",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE id IS NULL OR NOT (((id IS NULL)))",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE id IS NOT NULL OR NOT (((id IS NOT NULL)))",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE id = 1 OR id <> 1", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE id != 1 OR 1 = id", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE status = 'disabled' OR status != 'disabled'",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE (id = 1 OR status = 'disabled') OR 1 != id OR id IS NULL",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE id > 1 OR id <= 1", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE 1 >= id OR id > 1", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE id >= 1 OR 1 > id", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE id IN (1) OR id NOT IN (1) OR id IS NULL",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE id IN (1, 2) OR id NOT IN (2, 1) OR id IS NULL",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE id BETWEEN 1 AND 2 OR id NOT BETWEEN 1 AND 2 OR id IS NULL",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE id = 1 OR (id <> 1 AND TRUE) OR id IS NULL",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE id IS NOT DISTINCT FROM id",
            DatabaseType::Postgres
        ));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE id <=> id", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database(
            "UPDATE users SET active = 0 WHERE name LIKE '%' OR name IS NULL",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "UPDATE users SET active = 0 WHERE name LIKE '%%' OR name IS NULL",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE (id IS NULL OR status = 'disabled') OR id IS NOT NULL",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database("UPDATE users SET active = 0 WHERE abs(1) = 1", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE lower('A') = 'a'", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE coalesce(NULL, 1) = 1", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE LOWER(_utf8mb4'A') = 'a'", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE EXTRACT(YEAR FROM DATE '2026-01-01') = 2026",
            DatabaseType::Postgres
        ));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE DATE '2026-01-01' < CURRENT_DATE",
            DatabaseType::Postgres
        ));
        assert!(is_dangerous_sql_for_database("DELETE FROM users WHERE USER = CURRENT_USER", DatabaseType::Postgres));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE id IN (SELECT id FROM archived_users)",
            DatabaseType::Postgres
        ));
        assert!(is_dangerous_sql_for_database(
            "DELETE FROM users WHERE EXISTS (SELECT 1 FROM archived_users)",
            DatabaseType::Postgres
        ));
        assert!(!is_dangerous_sql_for_database("DELETE FROM users WHERE id = 1", DatabaseType::Mysql));
        assert!(!is_dangerous_sql_for_database("UPDATE users SET active = 0 WHERE id = 1", DatabaseType::Mysql));
        assert!(!is_dangerous_sql_for_database("UPDATE users SET active = 0 WHERE abs(id) = 1", DatabaseType::Mysql));
        assert!(!is_dangerous_sql_for_database(
            "DELETE FROM users WHERE lower(email) = 'disabled@example.com'",
            DatabaseType::Mysql
        ));
        assert!(!is_dangerous_sql_for_database(
            "DELETE FROM users WHERE EXTRACT(YEAR FROM created_at) = 2026",
            DatabaseType::Postgres
        ));
        assert!(!is_dangerous_sql_for_database(
            "DELETE FROM users WHERE id IS NULL OR status IS NOT NULL",
            DatabaseType::Mysql
        ));
        assert!(!is_dangerous_sql_for_database(
            "DELETE FROM users WHERE (id IS NULL OR NOT (((id IS NULL)))) AND tenant_id = 1",
            DatabaseType::Mysql
        ));
        assert!(!is_dangerous_sql_for_database(
            "DELETE FROM users WHERE (id = 1 OR id <> 1) AND tenant_id = 1",
            DatabaseType::Mysql
        ));
        assert!(!is_dangerous_sql_for_database(
            "DELETE FROM users WHERE status = 'pending' OR status <> 'disabled'",
            DatabaseType::Mysql
        ));
        assert!(!is_dangerous_sql_for_database("DELETE FROM users WHERE id IN (1) OR id IN (2)", DatabaseType::Mysql));
        assert!(!is_dangerous_sql_for_database(
            "DELETE FROM users WHERE id BETWEEN 1 AND 2 OR id BETWEEN 4 AND 5",
            DatabaseType::Mysql
        ));
        assert!(!is_dangerous_sql_for_database(
            "UPDATE users SET active = 0 WHERE name LIKE 'admin%'",
            DatabaseType::Mysql
        ));
        assert!(!is_dangerous_sql_for_database(
            "UPDATE users SET active = 0 WHERE status = 'inactive' AND tenant_id = 1",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "UPDATE users JOIN accounts ON accounts.id = users.account_id SET users.active = 0 WHERE users.id = 1",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "UPDATE users SET active = false FROM accounts WHERE users.account_id = accounts.id",
            DatabaseType::Postgres
        ));
        assert!(is_dangerous_sql_for_database(
            "DELETE users FROM users JOIN accounts ON accounts.id = users.account_id WHERE users.id = 1",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database("REPLACE INTO users (id) VALUES (1)", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database(
            "INSERT INTO users (id) VALUES (1) ON DUPLICATE KEY UPDATE active = 1",
            DatabaseType::Mysql
        ));
        assert!(is_dangerous_sql_for_database(
            "INSERT INTO users (id) VALUES (1) ON CONFLICT (id) DO UPDATE SET active = true",
            DatabaseType::Postgres
        ));
        assert!(!is_dangerous_sql_for_database("INSERT INTO users (id) VALUES (1)", DatabaseType::Mysql));
        assert!(is_dangerous_sql_for_database(
            "INSERT INTO users (id) SELECT id FROM staged_users",
            DatabaseType::Mysql
        ));
        assert!(!is_dangerous_sql_for_database(
            "INSERT INTO users (id) VALUES (1) ON CONFLICT (id) DO NOTHING",
            DatabaseType::Postgres
        ));
    }

    #[test]
    fn select_into_requires_central_high_risk_permission_for_supported_databases() {
        for database_type in [
            DatabaseType::Postgres,
            DatabaseType::Redshift,
            DatabaseType::Gaussdb,
            DatabaseType::OpenGauss,
            DatabaseType::Kingbase,
            DatabaseType::Highgo,
            DatabaseType::Vastbase,
            DatabaseType::Kwdb,
        ] {
            assert!(
                is_dangerous_sql_for_database("SELECT * INTO copied_users FROM users", database_type),
                "expected PostgreSQL-family SELECT INTO to require high-risk permission for {database_type:?}"
            );
        }
        assert!(is_dangerous_sql_for_database("SELECT * INTO #copied_users FROM users", DatabaseType::SqlServer));
        assert!(is_dangerous_sql_for_database("SELECT 1 INTO OUTFILE '/tmp/dbx-probe.txt'", DatabaseType::Mysql));
        assert!(!is_dangerous_sql_for_database("SELECT 1", DatabaseType::Postgres));
        assert!(!is_dangerous_sql_for_database("SELECT 1 INTO unsupported", DatabaseType::Sqlite));
    }

    #[test]
    fn classify_transaction_statements() {
        assert_eq!(classify_sql_risk("BEGIN", "postgres").unwrap(), SqlRisk::Transaction);
        assert_eq!(classify_sql_risk("COMMIT", "postgres").unwrap(), SqlRisk::Transaction);
        assert_eq!(classify_sql_risk("ROLLBACK", "postgres").unwrap(), SqlRisk::Transaction);
    }

    #[test]
    fn classify_multi_statement_returns_highest_risk() {
        // SELECT + INSERT = Write
        assert_eq!(classify_sql_risk("SELECT 1; INSERT INTO users VALUES (1)", "postgres").unwrap(), SqlRisk::Write);
    }

    #[test]
    fn classify_fallback_on_parse_error() {
        // Non-standard SQL should fall back to keyword matching
        assert_eq!(classify_sql_risk("SELECT * FROM users", "generic").unwrap(), SqlRisk::ReadOnly);
    }

    #[test]
    fn classify_unknown_statement_is_write() {
        // Statements not explicitly handled should be conservative (Write)
        // This depends on sqlparser's coverage, but we can test the catch-all
        assert_eq!(classify_sql_risk("GRANT SELECT ON users TO admin", "postgres").unwrap(), SqlRisk::Ddl);
    }

    #[test]
    fn classifies_search_engine_rest_risk_by_method_and_path() {
        for database_type in [DatabaseType::Elasticsearch, DatabaseType::Easysearch] {
            assert_eq!(
                classify_sql_risk_for_database("GET /_cluster/health", database_type).unwrap(),
                SqlRisk::ReadOnly
            );
            assert_eq!(
                classify_sql_risk_for_database("POST /products/_search\n{}", database_type).unwrap(),
                SqlRisk::ReadOnly
            );
            assert_eq!(
                classify_sql_risk_for_database("PUT /products/_doc/1\n{}", database_type).unwrap(),
                SqlRisk::Write
            );
            assert!(!is_dangerous_sql_for_database("PUT /products/_doc/1\n{}", database_type));
            assert_eq!(classify_sql_risk_for_database("DELETE /products", database_type).unwrap(), SqlRisk::Ddl);
            assert!(is_dangerous_sql_for_database("DELETE /products", database_type));
        }
    }

    #[test]
    fn side_effect_selects_are_not_read_only() {
        for (sql, database_type) in [
            ("SELECT pg_terminate_backend(123)", DatabaseType::Postgres),
            ("SELECT pg_catalog.pg_cancel_backend(123)", DatabaseType::Postgres),
            ("SELECT * FROM (SELECT pg_terminate_backend(1)) AS x", DatabaseType::Postgres),
            ("SELECT * FROM dblink('host=x', 'DELETE FROM t RETURNING 1') AS t(x int)", DatabaseType::Postgres),
            ("SELECT dblink_exec('host=x', 'DROP TABLE t')", DatabaseType::Postgres),
            ("SELECT lo_export(16384, '/tmp/x')", DatabaseType::Postgres),
            ("SELECT pg_read_file('/etc/passwd')", DatabaseType::Postgres),
            ("SELECT pg_ls_dir('.')", DatabaseType::Postgres),
            ("SELECT set_config('search_path', 'evil', false)", DatabaseType::Postgres),
            ("SELECT pg_advisory_lock(1)", DatabaseType::Postgres),
            ("SELECT pg_notify('channel', 'payload')", DatabaseType::Postgres),
            ("SELECT pg_create_logical_replication_slot('s', 'pgoutput')", DatabaseType::Postgres),
            ("SELECT pg_switch_wal()", DatabaseType::Postgres),
            ("SELECT sys_terminate_backend(123)", DatabaseType::Kingbase),
            ("SELECT SLEEP(5)", DatabaseType::Mysql),
            ("SELECT BENCHMARK(1000000, MD5('x'))", DatabaseType::Mysql),
            ("SELECT GET_LOCK('x', 10)", DatabaseType::Mysql),
            ("SELECT LOAD_FILE('/etc/passwd')", DatabaseType::Mysql),
            ("SELECT sys_exec('id')", DatabaseType::Mysql),
            ("SELECT * FROM OPENROWSET('SQLNCLI', 'Server=x;', 'SELECT 1')", DatabaseType::SqlServer),
            ("SELECT UTL_HTTP.REQUEST('http://attacker/') FROM dual", DatabaseType::Oracle),
            ("SELECT DBMS_PIPE.RECEIVE_MESSAGE('x', 10) FROM dual", DatabaseType::Oracle),
            ("SELECT load_extension('/tmp/evil')", DatabaseType::Sqlite),
            ("SELECT * FROM url('http://internal/', CSV)", DatabaseType::ClickHouse),
        ] {
            assert_ne!(
                classify_sql_risk_for_database(sql, database_type).unwrap(),
                SqlRisk::ReadOnly,
                "expected side-effect SELECT to be write-capable: {sql}"
            );
        }
        // Read-only Oracle package helpers stay reads.
        assert_eq!(
            classify_sql_risk_for_database(
                "SELECT DBMS_METADATA.GET_DDL('TABLE', 'T') FROM dual",
                DatabaseType::Oracle
            )
            .unwrap(),
            SqlRisk::ReadOnly
        );
        assert_eq!(
            classify_sql_risk_for_database(
                "SELECT lower(name), count(*) FROM users GROUP BY 1",
                DatabaseType::Postgres
            )
            .unwrap(),
            SqlRisk::ReadOnly
        );
    }

    #[test]
    fn keyword_fallback_still_sees_side_effect_functions() {
        // Unparseable text used to fall back to the keyword classifier, which answered ReadOnly.
        let sql = "SELECT pg_terminate_backend(1) FROM pg_stat_activity WHERE )(";
        assert!(Parser::parse_sql(&PostgreSqlDialect {}, sql).is_err());
        assert_eq!(classify_sql_risk_for_database(sql, DatabaseType::Postgres).unwrap(), SqlRisk::Write);
        assert!(is_dangerous_sql_for_database(sql, DatabaseType::Postgres));
        assert_eq!(classify_sql_risk("SELECT sleep(10) FROM t WHERE )(", "mysql").unwrap(), SqlRisk::Write);
    }

    #[test]
    fn strict_check_requires_allowlisted_functions_on_mysql_and_postgres() {
        for (sql, database_type) in [
            ("SELECT * FROM users", DatabaseType::Mysql),
            ("SELECT COUNT(*), SUM(total), MAX(created_at) FROM orders", DatabaseType::Mysql),
            ("SELECT CONCAT(first_name, ' ', last_name), IFNULL(nick, '-') FROM users", DatabaseType::Mysql),
            ("SELECT lower(email), now(), coalesce(a, b) FROM users", DatabaseType::Postgres),
            ("SELECT pg_catalog.lower('A')", DatabaseType::Postgres),
            ("SELECT * FROM generate_series(1, 3)", DatabaseType::Postgres),
            ("SELECT id, row_number() OVER (ORDER BY id) FROM t", DatabaseType::Postgres),
            ("SELECT my_udf(1)", DatabaseType::Sqlite),
            ("SELECT dbo.fn_total(1)", DatabaseType::SqlServer),
            ("GET key", DatabaseType::Redis),
        ] {
            assert_eq!(strict_read_only_violation_for_database(sql, database_type), None, "expected proven: {sql}");
        }
        for (sql, database_type) in [
            ("SELECT my_udf(1)", DatabaseType::Mysql),
            ("SELECT app.audit_touch(id) FROM users", DatabaseType::Mysql),
            ("SELECT @a := 1", DatabaseType::Mysql),
            ("SELECT id FROM users LIMIT 1 INTO @id", DatabaseType::Mysql),
            ("SELECT SLEEP(1)", DatabaseType::Mysql),
            ("SELECT my_udf()", DatabaseType::Postgres),
            ("SELECT public.lower('A')", DatabaseType::Postgres),
            ("SELECT * FROM my_set_returning_fn()", DatabaseType::Postgres),
            ("SELECT * FROM t WHERE id IN (SELECT writer_fn(1))", DatabaseType::Postgres),
            ("SELECT nextval('seq')", DatabaseType::Postgres),
            ("SELECT load_extension('/tmp/evil')", DatabaseType::Sqlite),
            ("SELECT * FROM OPENQUERY(remote, 'SELECT 1')", DatabaseType::SqlServer),
            ("SELECT UTL_HTTP.REQUEST('http://attacker/') FROM dual", DatabaseType::Oracle),
            // Parse failure: the token-level fallback still applies the allowlist.
            ("SELECT my_udf(1) FROM FROM t", DatabaseType::Mysql),
        ] {
            assert!(strict_read_only_violation_for_database(sql, database_type).is_some(), "expected unproven: {sql}");
        }
        // The token fallback ignores clause keywords and cast type names before `(`.
        assert_eq!(
            strict_read_only_violation_for_database(
                "SELECT COUNT(*) FROM t WHERE id IN (1, 2) AND CAST(x AS DECIMAL(10, 2)) > 0 FROM",
                DatabaseType::Mysql
            ),
            None
        );
    }
}
