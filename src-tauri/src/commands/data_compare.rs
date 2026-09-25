use std::sync::Arc;
use tauri::State;

use crate::commands::connection::AppState;

/// Row diffing is CPU-heavy; synchronous commands run on the main thread and
/// would freeze the UI, so the work runs on the blocking pool.
async fn run_blocking<T: Send + 'static>(job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(job).await.map_err(|error| format!("Data compare task failed: {error}"))?
}

#[tauri::command]
pub async fn prepare_data_compare(
    options: dbx_core::data_compare::DataComparePreparationOptions,
) -> Result<dbx_core::data_compare::DataComparePreparation, String> {
    run_blocking(move || dbx_core::data_compare::prepare_data_compare(options)).await
}

#[tauri::command]
pub async fn prepare_data_compare_from_tables(
    state: State<'_, Arc<AppState>>,
    options: dbx_core::data_compare::DataCompareFromTablesOptions,
) -> Result<dbx_core::data_compare::DataCompareFromTablesPreparation, String> {
    dbx_core::data_compare::prepare_data_compare_from_tables(&state, options).await
}

#[tauri::command]
pub async fn prepare_data_compare_missing_target(
    state: State<'_, Arc<AppState>>,
    options: dbx_core::data_compare::DataCompareMissingTargetOptions,
) -> Result<dbx_core::data_compare::DataCompareFromTablesPreparation, String> {
    dbx_core::data_compare::prepare_data_compare_missing_target(&state, options).await
}

#[tauri::command]
pub async fn build_data_compare_sync_plan(
    options: dbx_core::data_compare::DataCompareSyncPlanOptions,
) -> Result<dbx_core::data_compare::DataCompareSyncPlan, String> {
    run_blocking(move || Ok(dbx_core::data_compare::build_data_compare_sync_plan(options))).await
}
