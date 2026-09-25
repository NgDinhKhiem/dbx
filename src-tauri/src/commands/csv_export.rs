use std::sync::Arc;

use crate::commands::connection::AppState;
use dbx_core::csv_export::{
    export_table_data_csv_core, write_query_result_csv_file, CsvQuoteMode, TableCsvExportOptions,
};
use serde::Deserialize;
use serde_json::Value;
use tauri::State;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResultCsvExportRequest {
    pub file_path: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    #[serde(default)]
    pub csv_quote_mode: CsvQuoteMode,
}

#[tauri::command]
pub async fn export_query_result_csv(request: QueryResultCsvExportRequest) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        write_query_result_csv_file(&request.file_path, &request.columns, &request.rows, request.csv_quote_mode)
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn export_table_data_csv(
    state: State<'_, Arc<AppState>>,
    request: TableCsvExportOptions,
) -> Result<u64, String> {
    export_table_data_csv_core(&state, request).await
}
