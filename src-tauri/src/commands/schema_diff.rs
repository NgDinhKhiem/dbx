/// Diff preparation and DDL generation are CPU-heavy on large schemas;
/// synchronous commands run on the main thread, so run them on the blocking pool.
async fn run_blocking<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(job).await.map_err(|error| format!("Schema diff task failed: {error}"))
}

#[tauri::command]
pub async fn prepare_schema_diff(
    options: dbx_core::schema_diff::SchemaDiffPreparationOptions,
) -> Result<dbx_core::schema_diff::SchemaDiffPreparation, String> {
    run_blocking(move || dbx_core::schema_diff::prepare_schema_diff(options)).await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn generate_schema_sync_sql(
    diffs: Vec<dbx_core::schema_diff::TableDiff>,
    function_diffs: Option<Vec<dbx_core::schema_diff::FunctionDiff>>,
    sequence_diffs: Option<Vec<dbx_core::schema_diff::SequenceDiff>>,
    rule_diffs: Option<Vec<dbx_core::schema_diff::RuleDiff>>,
    owner_diffs: Option<Vec<dbx_core::schema_diff::OwnerDiff>>,
    database_type: dbx_core::models::connection::DatabaseType,
    target_schema: Option<String>,
    cascade_delete: Option<bool>,
    source_dialect: Option<dbx_core::sql_dialect::descriptor::DialectKind>,
    field_mappings: Option<Vec<dbx_core::schema_diff::FieldMapping>>,
) -> Result<String, String> {
    run_blocking(move || {
        dbx_core::schema_diff::generate_schema_sync_sql(
            &diffs,
            function_diffs.as_deref().unwrap_or_default(),
            sequence_diffs.as_deref().unwrap_or_default(),
            rule_diffs.as_deref().unwrap_or_default(),
            owner_diffs.as_deref().unwrap_or_default(),
            database_type,
            target_schema.as_deref(),
            cascade_delete.unwrap_or(false),
            source_dialect,
            &field_mappings.unwrap_or_default(),
        )
    })
    .await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn generate_schema_sync_plan(
    diffs: Vec<dbx_core::schema_diff::TableDiff>,
    function_diffs: Option<Vec<dbx_core::schema_diff::FunctionDiff>>,
    sequence_diffs: Option<Vec<dbx_core::schema_diff::SequenceDiff>>,
    rule_diffs: Option<Vec<dbx_core::schema_diff::RuleDiff>>,
    owner_diffs: Option<Vec<dbx_core::schema_diff::OwnerDiff>>,
    database_type: dbx_core::models::connection::DatabaseType,
    target_schema: Option<String>,
    cascade_delete: Option<bool>,
    source_dialect: Option<dbx_core::sql_dialect::descriptor::DialectKind>,
    field_mappings: Option<Vec<dbx_core::schema_diff::FieldMapping>>,
    enable_rollback: Option<bool>,
) -> Result<dbx_core::schema_diff::SchemaSyncSqlPlan, String> {
    run_blocking(move || {
        dbx_core::schema_diff::generate_schema_sync_sql_plan(
            &diffs,
            function_diffs.as_deref().unwrap_or_default(),
            sequence_diffs.as_deref().unwrap_or_default(),
            rule_diffs.as_deref().unwrap_or_default(),
            owner_diffs.as_deref().unwrap_or_default(),
            database_type,
            target_schema.as_deref(),
            cascade_delete.unwrap_or(false),
            source_dialect,
            &field_mappings.unwrap_or_default(),
            enable_rollback.unwrap_or(false),
        )
    })
    .await
}
