//! Read-only OpenSearch / Elasticsearch tools (`dbx_opensearch_*`) with time
//! windows, modelled on OpenSearch Dashboards Discover.
//!
//! Declared as a child module of `server` so it reuses the server's policy
//! helpers (tool allow-list, connection/group scope). Every request goes
//! through the backend's read-only `elasticsearch_read_request`; request
//! building, DQL and response shaping live in
//! `dbx_core::db::{elasticsearch_dql, elasticsearch_mcp}`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use dbx_core::db::elasticsearch_driver::{opensearch_query_endpoint_missing, ElasticsearchRawResponse};
use dbx_core::db::elasticsearch_mcp::{
    self as es, build_query_clause, build_search_body, clamp_limit, compact_hits, dql_wildcard_fields,
    encode_index_pattern, error_reason, fit_response, insert_ppl_time_window, known_dql_fields, parse_field_caps,
    parse_saved_index_patterns, read_total_hits, resolve_time_window, shape_tabular_response, summarize_cat_indices,
    summarize_resolve_index, validate_select_sql, FieldCapability, QueryLanguage, SavedIndexPattern, TimeWindow,
    DEFAULT_TIME_FIELD, MAX_RESPONSE_BYTES,
};
use dbx_core::models::connection::{ConnectionConfig, DatabaseType};
use rmcp::{handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{backend_tool_error, text, tool_error, ConnectionSelector, DbxMcpServer};

/// Every tool this module registers, in registration order.
#[cfg(test)]
pub(crate) const OPENSEARCH_TOOL_NAMES: &[&str] = &[
    "dbx_opensearch_list_indices",
    "dbx_opensearch_resolve_pattern",
    "dbx_opensearch_fields",
    "dbx_opensearch_search",
    "dbx_opensearch_sql",
    "dbx_opensearch_ppl",
];

const SAVED_PATTERN_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenSearchConnectionRequest {
    #[serde(flatten)]
    pub selector: ConnectionSelector,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenSearchPatternRequest {
    #[serde(flatten)]
    pub selector: ConnectionSelector,
    #[schemars(description = "Index, alias or pattern, e.g. `logs-*` (comma-separate several)")]
    pub pattern: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenSearchSearchRequest {
    #[serde(flatten)]
    pub selector: ConnectionSelector,
    #[schemars(description = "Index, alias or pattern, e.g. `logs-local-*`")]
    pub pattern: String,
    #[schemars(
        description = "DQL such as `level:error and not status:200` or `kubernetes.*:web`, or Lucene when language=lucene. Empty matches every document in the window.",
        extend("type" = "string")
    )]
    pub query: Option<String>,
    #[schemars(description = "Query language: dql (default) or lucene", extend("type" = "string", "enum" = ["dql", "lucene"]))]
    pub language: Option<String>,
    #[schemars(
        description = "Start of the time window: now-15m, now-1h, now-24h, now-7d (now±N with s/m/h/d/w/M/y) or an absolute ISO timestamp (UTC when no zone). Default now-1h.",
        extend("type" = "string")
    )]
    pub from: Option<String>,
    #[schemars(description = "End of the time window: now (default) or an absolute ISO timestamp", extend("type" = "string"))]
    pub to: Option<String>,
    #[schemars(
        description = "Date field the window applies to. Default: the saved index pattern's time field, else @timestamp.",
        extend("type" = "string")
    )]
    pub time_field: Option<String>,
    #[schemars(
        description = "Search every document regardless of time. Only for indices without a time field.",
        extend("type" = "boolean")
    )]
    pub all_time: Option<bool>,
    #[schemars(description = "Maximum documents to return (default 20, max 200)", extend("type" = "integer"))]
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenSearchSqlRequest {
    #[serde(flatten)]
    pub selector: ConnectionSelector,
    #[schemars(
        description = "One SELECT statement; quote index names with backticks, e.g. SELECT level, count(*) FROM `logs-*` GROUP BY level"
    )]
    pub query: String,
    #[schemars(description = "Maximum rows to return (default 20, max 200)", extend("type" = "integer"))]
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenSearchPplRequest {
    #[serde(flatten)]
    pub selector: ConnectionSelector,
    #[schemars(description = "PPL starting with source=, e.g. source=logs-* | stats count() by level")]
    pub query: String,
    #[schemars(
        description = "Start of the time window: now-15m, now-1h, now-24h, now-7d or an absolute ISO timestamp. Default now-1h.",
        extend("type" = "string")
    )]
    pub from: Option<String>,
    #[schemars(description = "End of the time window: now (default) or an absolute ISO timestamp", extend("type" = "string"))]
    pub to: Option<String>,
    #[schemars(
        description = "Date field the window applies to. Default: the saved index pattern's time field, else @timestamp.",
        extend("type" = "string")
    )]
    pub time_field: Option<String>,
    #[schemars(
        description = "Run without a time window. Only for indices without a time field.",
        extend("type" = "boolean")
    )]
    pub all_time: Option<bool>,
    #[schemars(description = "Maximum rows to return (default 20, max 200)", extend("type" = "integer"))]
    pub limit: Option<u64>,
}

fn optional_text(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|value| !value.is_empty())
}

fn database_type_name(db_type: DatabaseType) -> String {
    serde_json::to_value(db_type)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{db_type:?}"))
}

/// Refusal for a non-search connection, pointing at the tools that fit it.
pub(crate) fn ensure_search_connection(connection: &ConnectionConfig) -> Result<(), String> {
    if matches!(connection.db_type, DatabaseType::Elasticsearch | DatabaseType::Easysearch) {
        return Ok(());
    }
    let pointer = match connection.db_type {
        DatabaseType::Redis => "Use dbx_execute_redis_command for Redis.",
        DatabaseType::MessageQueue => "Use dbx_peek_messages (and the dbx_kafka_* tools) for message queues.",
        DatabaseType::MongoDb => "Use dbx_execute_query with MongoDB shell syntax for MongoDB.",
        _ => "Use dbx_list_tables, dbx_describe_table and dbx_execute_query for this connection.",
    };
    Err(format!(
        "Connection \"{}\" is a {} connection. dbx_opensearch_* tools only work with Elasticsearch / OpenSearch / \
         Easysearch connections (dbx_list_connections shows each type). {pointer}",
        connection.name,
        database_type_name(connection.db_type)
    ))
}

type SavedPatterns = Result<Vec<SavedIndexPattern>, String>;

fn saved_pattern_cache() -> &'static Mutex<HashMap<String, (Instant, SavedPatterns)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, SavedPatterns)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn saved_pattern_cache_key(connection: &ConnectionConfig) -> String {
    format!("{}\u{0}{}\u{0}{}", connection.id, connection.host, connection.port)
}

fn is_success(response: &ElasticsearchRawResponse) -> bool {
    (200..300).contains(&response.status)
}

fn parse_body(response: &ElasticsearchRawResponse) -> Result<Value, String> {
    serde_json::from_str(&response.body).map_err(|error| format!("Invalid JSON from the cluster: {error}"))
}

fn json_result(value: Value, array_key: &str) -> CallToolResult {
    text(fit_response(value, array_key, MAX_RESPONSE_BYTES))
}

fn request_error(error: String) -> CallToolResult {
    backend_tool_error("OPENSEARCH_REQUEST_ERROR", error)
}

// CallToolResult is the rmcp wire response type, as in the server helpers.
#[allow(clippy::result_large_err)]
fn pattern_arg(pattern: &str) -> Result<(String, String), CallToolResult> {
    let trimmed = pattern.trim().to_string();
    encode_index_pattern(&trimmed)
        .map(|encoded| (trimmed, encoded))
        .map_err(|error| tool_error("INVALID_INDEX_PATTERN", error))
}

fn time_field_note(fields: &[FieldCapability], time_field: &str) -> Option<String> {
    match fields.iter().find(|field| field.name == time_field) {
        None => Some(format!(
            "The time field \"{time_field}\" is not mapped in this pattern, so the time filter matches nothing. \
             Pass time_field (date fields: {}) or all_time=true for indices without a time field.",
            date_field_list(fields)
        )),
        Some(field) if !field.is_date() => Some(format!(
            "The time field \"{time_field}\" has type {}, not date; the window may not apply as expected. Date fields: {}.",
            field.field_type,
            date_field_list(fields)
        )),
        Some(_) => None,
    }
}

fn date_field_list(fields: &[FieldCapability]) -> String {
    let dates: Vec<&str> = fields.iter().filter(|field| field.is_date()).map(|field| field.name.as_str()).collect();
    if dates.is_empty() {
        "none".to_string()
    } else {
        dates.join(", ")
    }
}

/// How a search/PPL call picks its window: explicit, saved pattern, or none.
struct WindowChoice {
    time_field: String,
    window: Option<TimeWindow>,
    notes: Vec<String>,
}

impl DbxMcpServer {
    /// Tool allow-list, connection/group scope, then the search-engine check.
    #[allow(clippy::result_large_err)]
    async fn opensearch_connection(
        &self,
        tool_name: &str,
        selector: &ConnectionSelector,
    ) -> Result<ConnectionConfig, CallToolResult> {
        self.ensure_tool_allowed(tool_name).await?;
        let resolved = self.resolve_connection(selector).await?;
        ensure_search_connection(&resolved.connection)
            .map_err(|error| tool_error("OPENSEARCH_CONNECTION_REQUIRED", error))?;
        Ok(resolved.connection)
    }

    async fn es_get(&self, connection: &ConnectionConfig, path: &str) -> Result<ElasticsearchRawResponse, String> {
        self.backend.elasticsearch_read_request(connection, "GET", path, None).await
    }

    async fn es_post(
        &self,
        connection: &ConnectionConfig,
        path: &str,
        body: &Value,
    ) -> Result<ElasticsearchRawResponse, String> {
        self.backend.elasticsearch_read_request(connection, "POST", path, Some(body.to_string())).await
    }

    /// Index patterns saved in Dashboards / Kibana. `Err` carries a note for
    /// the response (no permission, no saved objects index, ...), never fails a tool.
    async fn saved_index_patterns(&self, connection: &ConnectionConfig) -> SavedPatterns {
        let key = saved_pattern_cache_key(connection);
        if let Some((at, cached)) = saved_pattern_cache().lock().ok().and_then(|cache| cache.get(&key).cloned()) {
            if at.elapsed() < SAVED_PATTERN_TTL {
                return cached;
            }
        }
        let result = match self
            .es_post(connection, &es::saved_index_patterns_path(), &es::saved_index_patterns_request_body())
            .await
        {
            Ok(response) if is_success(&response) => {
                parse_body(&response).map(|body| parse_saved_index_patterns(&body))
            }
            Ok(response) if matches!(response.status, 401 | 403) => Err(
                "This account may not read the Dashboards saved objects index (.kibana), so saved index patterns are \
                 not listed."
                    .to_string(),
            ),
            Ok(response) if response.status == 404 => {
                Err("No Dashboards / Kibana saved objects index (.kibana) was found.".to_string())
            }
            Ok(response) => {
                Err(format!("Could not read saved index patterns: {}", error_reason(response.status, &response.body)))
            }
            // Transport failures are not cached; the next call retries.
            Err(error) => return Err(format!("Could not read saved index patterns: {error}")),
        };
        if let Ok(mut cache) = saved_pattern_cache().lock() {
            cache.insert(key, (Instant::now(), result.clone()));
        }
        result
    }

    async fn saved_pattern_for(&self, connection: &ConnectionConfig, pattern: &str) -> Option<SavedIndexPattern> {
        self.saved_index_patterns(connection).await.ok()?.into_iter().find(|saved| saved.title == pattern)
    }

    async fn field_caps(
        &self,
        connection: &ConnectionConfig,
        encoded_pattern: &str,
        fields: &str,
    ) -> Result<(Value, Vec<FieldCapability>), String> {
        let response = self.es_get(connection, &es::field_caps_path(encoded_pattern, fields)).await?;
        if !is_success(&response) {
            return Err(error_reason(response.status, &response.body));
        }
        let body = parse_body(&response)?;
        let fields = parse_field_caps(&body);
        Ok((body, fields))
    }

    /// Resolve the time field and window for search / PPL.
    #[allow(clippy::too_many_arguments, clippy::result_large_err)]
    async fn window_choice(
        &self,
        connection: &ConnectionConfig,
        pattern: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        time_field: Option<&str>,
        all_time: bool,
    ) -> Result<WindowChoice, CallToolResult> {
        let mut notes = Vec::new();
        let saved = match (time_field, pattern) {
            (None, Some(pattern)) if !all_time => self.saved_pattern_for(connection, pattern).await,
            _ => None,
        };
        let time_field = time_field
            .map(str::to_string)
            .or_else(|| saved.as_ref().and_then(|saved| saved.time_field.clone()))
            .unwrap_or_else(|| DEFAULT_TIME_FIELD.to_string());
        if all_time {
            if from.is_some() || to.is_some() {
                notes.push("all_time=true: from / to were ignored.".to_string());
            }
            return Ok(WindowChoice { time_field, window: None, notes });
        }
        if let Some(saved) = saved.as_ref().filter(|saved| saved.time_field.is_none()) {
            notes.push(format!(
                "The saved index pattern \"{}\" has no time field, so no time window was applied.",
                saved.title
            ));
            return Ok(WindowChoice { time_field, window: None, notes });
        }
        if saved.is_some() {
            notes.push(format!("Time field \"{time_field}\" comes from the saved index pattern."));
        }
        let window = resolve_time_window(from, to, chrono::Utc::now())
            .map_err(|error| tool_error("INVALID_TIME_RANGE", error))?;
        Ok(WindowChoice { time_field, window: Some(window), notes })
    }
}

#[tool_router(router = opensearch_tool_router, vis = "pub(crate)")]
impl DbxMcpServer {
    #[tool(
        name = "dbx_opensearch_list_indices",
        description = "List index patterns saved in OpenSearch Dashboards / Kibana (with their time field) first, then concrete indices (health, docs, size) and suggested rollover patterns. Elasticsearch / OpenSearch / Easysearch connections only. Next: dbx_opensearch_fields, then dbx_opensearch_search."
    )]
    async fn opensearch_list_indices(
        &self,
        Parameters(request): Parameters<OpenSearchConnectionRequest>,
    ) -> CallToolResult {
        let connection = match self.opensearch_connection("dbx_opensearch_list_indices", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        let saved = self.saved_index_patterns(&connection).await;
        let response = match self
            .es_get(&connection, "/_cat/indices?format=json&h=index,health,status,docs.count,store.size&s=index")
            .await
        {
            Ok(response) => response,
            Err(error) => return request_error(error),
        };
        if !is_success(&response) {
            return tool_error("OPENSEARCH_REQUEST_ERROR", error_reason(response.status, &response.body));
        }
        let body = match parse_body(&response) {
            Ok(body) => body,
            Err(error) => return request_error(error),
        };
        let (indices, hidden, derived) = summarize_cat_indices(&body);
        let mut result = json!({
            "index_patterns": saved.as_ref().map(|patterns| patterns.iter().map(SavedIndexPattern::to_json).collect::<Vec<_>>()).unwrap_or_default(),
        });
        if let Err(note) = &saved {
            result["index_patterns_note"] = json!(note);
        }
        result["suggested_patterns"] = json!(derived);
        result["index_count"] = json!(indices.len());
        result["hidden_indices_omitted"] = json!(hidden);
        result["indices"] = json!(indices);
        result["next"] = json!(
            "Call dbx_opensearch_fields(pattern) to see searchable fields and the time field, then dbx_opensearch_search."
        );
        json_result(result, "indices")
    }

    #[tool(
        name = "dbx_opensearch_resolve_pattern",
        description = "Expand an index pattern (e.g. logs-*) into the concrete indices, aliases and data streams it matches. Elasticsearch / OpenSearch / Easysearch connections only."
    )]
    async fn opensearch_resolve_pattern(
        &self,
        Parameters(request): Parameters<OpenSearchPatternRequest>,
    ) -> CallToolResult {
        let connection = match self.opensearch_connection("dbx_opensearch_resolve_pattern", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        let (pattern, encoded) = match pattern_arg(&request.pattern) {
            Ok(pattern) => pattern,
            Err(error) => return error,
        };
        let response = match self.es_get(&connection, &format!("/_resolve/index/{encoded}?expand_wildcards=open")).await
        {
            Ok(response) => response,
            Err(error) => return request_error(error),
        };
        let mut result = if is_success(&response) {
            match parse_body(&response) {
                Ok(body) => summarize_resolve_index(&body),
                Err(error) => return request_error(error),
            }
        } else if response.status == 404 && response.body.contains("index_not_found") {
            json!({ "indices": [], "aliases": [], "data_streams": [] })
        } else {
            // Clusters without `_resolve/index` (Elasticsearch < 7.9): fall back to `_cat`.
            let cat = match self
                .es_get(&connection, &format!("/_cat/indices/{encoded}?format=json&h=index,health,status,docs.count"))
                .await
            {
                Ok(cat) if is_success(&cat) => cat,
                Ok(cat) => return tool_error("OPENSEARCH_REQUEST_ERROR", error_reason(cat.status, &cat.body)),
                Err(error) => return request_error(error),
            };
            let (indices, hidden, _) = summarize_cat_indices(&parse_body(&cat).unwrap_or(Value::Null));
            let aliases =
                match self.es_get(&connection, &format!("/_cat/aliases/{encoded}?format=json&h=alias,index")).await {
                    Ok(aliases) if is_success(&aliases) => parse_body(&aliases).unwrap_or(Value::Null),
                    _ => Value::Null,
                };
            json!({ "indices": indices, "hidden_indices_omitted": hidden, "aliases": aliases.as_array().cloned().unwrap_or_default() })
        };
        let count = result["indices"].as_array().map_or(0, Vec::len);
        result["pattern"] = json!(pattern);
        result["index_count"] = json!(count);
        if count == 0 && result["aliases"].as_array().is_none_or(Vec::is_empty) {
            result["note"] =
                json!("No index or alias matches this pattern. dbx_opensearch_list_indices lists what exists.");
        }
        if let Some(saved) = self.saved_pattern_for(&connection, &pattern).await {
            result["saved_index_pattern"] = saved.to_json();
        }
        json_result(result, "indices")
    }

    #[tool(
        name = "dbx_opensearch_fields",
        description = "Fields of an index pattern (via _field_caps) with type, searchable and aggregatable, plus date fields and the default time field. Check this before searching: querying a field that is not searchable (mapped with index: false) fails the whole search. Use a .keyword sub-field for exact matches and aggregations."
    )]
    async fn opensearch_fields(&self, Parameters(request): Parameters<OpenSearchPatternRequest>) -> CallToolResult {
        let connection = match self.opensearch_connection("dbx_opensearch_fields", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        let (pattern, encoded) = match pattern_arg(&request.pattern) {
            Ok(pattern) => pattern,
            Err(error) => return error,
        };
        let (body, fields) = match self.field_caps(&connection, &encoded, "*").await {
            Ok(result) => result,
            Err(error) => return tool_error("OPENSEARCH_REQUEST_ERROR", error),
        };
        let saved = self.saved_pattern_for(&connection, &pattern).await;
        let date_fields: Vec<&str> =
            fields.iter().filter(|field| field.is_date()).map(|field| field.name.as_str()).collect();
        let default_time_field = saved
            .as_ref()
            .and_then(|saved| saved.time_field.clone())
            .or_else(|| date_fields.iter().find(|name| **name == DEFAULT_TIME_FIELD).map(|name| name.to_string()))
            .or_else(|| date_fields.first().map(|name| name.to_string()));
        let not_searchable: Vec<&str> =
            fields.iter().filter(|field| !field.searchable).map(|field| field.name.as_str()).collect();
        let mut result = json!({
            "pattern": pattern,
            "indices": body.get("indices").and_then(Value::as_array).map_or(0, Vec::len),
            "field_count": fields.len(),
            "default_time_field": default_time_field,
            "date_fields": date_fields,
            "not_searchable": not_searchable,
        });
        if let Some(saved) = saved {
            result["saved_index_pattern"] = saved.to_json();
        }
        if fields.is_empty() {
            result["note"] = json!("No fields found: the pattern matches no index, or the indices have no mapping.");
        }
        result["hint"] = json!(
            "Only query fields with searchable=true; a query on a non-searchable field fails the whole search. Use .keyword sub-fields for exact values and aggregations."
        );
        result["fields"] = json!(fields.iter().map(FieldCapability::to_json).collect::<Vec<_>>());
        json_result(result, "fields")
    }

    #[tool(
        name = "dbx_opensearch_search",
        description = "Search an index or pattern with Dashboards Query Language (default) or Lucene, within a time window. Log indices are time-series: pass from / to (now-15m, now-1h, now-24h, now-7d or absolute ISO timestamps); when omitted the window is the last hour on the saved pattern's time field or @timestamp. The response states the exact window used (UTC). An empty query matches every document in the window. Use all_time=true only for indices without a time field. Check dbx_opensearch_fields first: querying a non-searchable field fails the search. Returns total hits and compact hits (_index, _id, time, _source with long values truncated), newest first."
    )]
    async fn opensearch_search(&self, Parameters(request): Parameters<OpenSearchSearchRequest>) -> CallToolResult {
        let connection = match self.opensearch_connection("dbx_opensearch_search", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        let (pattern, encoded) = match pattern_arg(&request.pattern) {
            Ok(pattern) => pattern,
            Err(error) => return error,
        };
        let language = match QueryLanguage::parse(request.language.as_deref()) {
            Ok(language) => language,
            Err(error) => return tool_error("INVALID_QUERY_LANGUAGE", error),
        };
        let query = request.query.as_deref().unwrap_or_default().trim().to_string();
        let limit = clamp_limit(request.limit);
        let all_time = request.all_time.unwrap_or(false);
        let choice = match self
            .window_choice(
                &connection,
                Some(&pattern),
                optional_text(&request.from),
                optional_text(&request.to),
                optional_text(&request.time_field),
                all_time,
            )
            .await
        {
            Ok(choice) => choice,
            Err(error) => return error,
        };
        let WindowChoice { time_field, window, mut notes } = choice;

        // Wildcard field names (`kubernetes.*:web`) expand against the pattern's fields.
        let mut known_fields = Vec::new();
        if language == QueryLanguage::Dql {
            let wildcard_fields = dql_wildcard_fields(&query);
            if !wildcard_fields.is_empty() {
                match self.field_caps(&connection, &encoded, &wildcard_fields.join(",")).await {
                    Ok((_, fields)) => known_fields = known_dql_fields(&fields),
                    Err(error) => notes.push(format!("Could not expand wildcard field names: {error}")),
                }
            }
        }
        let clause = match build_query_clause(language, &query, &known_fields) {
            Ok(clause) => clause,
            Err(error) => {
                return tool_error("DQL_SYNTAX_ERROR", format!("{}\n{}", error.message, error.pointer()));
            }
        };
        let sort_field =
            (window.is_some() || optional_text(&request.time_field).is_some()).then_some(time_field.as_str());
        let body =
            build_search_body(clause, window.as_ref().map(|window| (time_field.as_str(), window)), sort_field, limit);
        let path = format!("/{encoded}/_search?ignore_unavailable=true&allow_no_indices=true");
        let response = match self.es_post(&connection, &path, &body).await {
            Ok(response) => response,
            Err(error) => return request_error(error),
        };
        if !is_success(&response) {
            let mut reason = error_reason(response.status, &response.body);
            let lower = reason.to_ascii_lowercase();
            if lower.contains("not indexed")
                || lower.contains("index: false")
                || lower.contains("cannot search on field")
            {
                reason.push_str(
                    "\nHint: a queried field is not searchable (index: false). Check dbx_opensearch_fields and query \
                     only searchable fields.",
                );
            }
            return tool_error("OPENSEARCH_SEARCH_ERROR", reason);
        }
        let result_body = match parse_body(&response) {
            Ok(body) => body,
            Err(error) => return request_error(error),
        };
        let (total, relation) = read_total_hits(&result_body);
        let hits = compact_hits(&result_body, sort_field);
        if total == 0 && window.is_some() {
            if let Ok((_, fields)) = self.field_caps(&connection, &encoded, &time_field).await {
                notes.extend(time_field_note(&fields, &time_field));
            }
        }
        let mut result = json!({
            "pattern": pattern,
            "language": language.as_str(),
            "query": query,
        });
        match &window {
            Some(window) => {
                result["time_window"] = window.describe(
                    &time_field,
                    optional_text(&request.from).unwrap_or(es::DEFAULT_WINDOW_FROM),
                    optional_text(&request.to).unwrap_or(es::DEFAULT_WINDOW_TO),
                )
            }
            None => result["time_window"] = json!("all time (no time filter)"),
        }
        result["total"] = json!({ "value": total, "relation": relation });
        result["returned"] = json!(hits.len());
        if let Some(took) = result_body.get("took") {
            result["took_ms"] = took.clone();
        }
        if !notes.is_empty() {
            result["notes"] = json!(notes);
        }
        result["hits"] = json!(hits);
        json_result(result, "hits")
    }

    #[tool(
        name = "dbx_opensearch_sql",
        description = "Run one read-only SQL SELECT on an OpenSearch (SQL plugin, JDBC format) or Elasticsearch (_sql) connection. Quote index names with backticks, e.g. SELECT level, count(*) FROM `logs-*` GROUP BY level. OpenSearch 1.x SQL has no NOW() or date math, so time bounds must be absolute UTC timestamps, e.g. WHERE `@timestamp` >= '2026-09-26 09:00:00'. Non-SELECT statements are rejected. Returns columns and rows (default 20, max 200)."
    )]
    async fn opensearch_sql(&self, Parameters(request): Parameters<OpenSearchSqlRequest>) -> CallToolResult {
        let connection = match self.opensearch_connection("dbx_opensearch_sql", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        let sql = match validate_select_sql(&request.query) {
            Ok(sql) => sql,
            Err(error) => return tool_error("SQL_BLOCKED", error),
        };
        let limit = clamp_limit(request.limit);
        let info = self.backend.elasticsearch_cluster_info(&connection).await.unwrap_or_default();
        let body = json!({ "query": sql });
        let (engine, response) = if info.is_opensearch() {
            let mut sent = self.es_post(&connection, "/_plugins/_sql?format=jdbc", &body).await;
            if let Ok(response) = &sent {
                if opensearch_query_endpoint_missing(response.status, &response.body) {
                    sent = self.es_post(&connection, "/_opendistro/_sql?format=jdbc", &body).await;
                }
            }
            ("opensearch", sent)
        } else {
            let mut sent = self.es_post(&connection, "/_sql", &body).await;
            // Elasticsearch 6.x registers `_sql` for GET/PUT only; GET with a body is the read form.
            if let Ok(response) = &sent {
                if response.status == 405 {
                    sent = self
                        .backend
                        .elasticsearch_read_request(&connection, "GET", "/_sql", Some(body.to_string()))
                        .await;
                }
            }
            (info.distribution.as_deref().unwrap_or("elasticsearch"), sent)
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => return request_error(error),
        };
        if !is_success(&response) {
            return tool_error("OPENSEARCH_SQL_ERROR", error_reason(response.status, &response.body));
        }
        let shaped = parse_body(&response).and_then(|body| shape_tabular_response(&body, limit));
        match shaped {
            Ok(mut shaped) => {
                shaped["engine"] = json!(engine);
                shaped["query"] = json!(sql);
                json_result(shaped, "rows")
            }
            Err(error) => request_error(error),
        }
    }

    #[tool(
        name = "dbx_opensearch_ppl",
        description = "Run a read-only Piped Processing Language query (OpenSearch only), e.g. source=logs-* | stats count() by level, within a time window. Pass from / to (now-15m, now-1h, now-24h, now-7d or absolute timestamps); when omitted the window is the last hour on the saved pattern's time field or @timestamp. The window is inserted as a where right after the source= command, since OpenSearch 1.x PPL has no NOW() or date math. The response states the exact window and the query that ran. all_time=true only for indices without a time field."
    )]
    async fn opensearch_ppl(&self, Parameters(request): Parameters<OpenSearchPplRequest>) -> CallToolResult {
        let connection = match self.opensearch_connection("dbx_opensearch_ppl", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        // Validate the query shape before any cluster round trip.
        if let Err(error) = insert_ppl_time_window(&request.query, DEFAULT_TIME_FIELD, None) {
            return tool_error("PPL_BLOCKED", error);
        }
        let info = self.backend.elasticsearch_cluster_info(&connection).await.unwrap_or_default();
        if info.distribution.is_some() && !info.is_opensearch() {
            return tool_error(
                "PPL_UNSUPPORTED",
                format!(
                    "PPL is only available on OpenSearch; this cluster is {}. Use dbx_opensearch_sql or \
                     dbx_opensearch_search instead.",
                    info.distribution.as_deref().unwrap_or("unknown")
                ),
            );
        }
        let limit = clamp_limit(request.limit);
        let source = es::ppl_source_pattern(&request.query);
        let choice = match self
            .window_choice(
                &connection,
                source.as_deref(),
                optional_text(&request.from),
                optional_text(&request.to),
                optional_text(&request.time_field),
                request.all_time.unwrap_or(false),
            )
            .await
        {
            Ok(choice) => choice,
            Err(error) => return error,
        };
        let WindowChoice { time_field, window, notes } = choice;
        let query = match insert_ppl_time_window(&request.query, &time_field, window.as_ref()) {
            Ok(query) => query,
            Err(error) => return tool_error("PPL_BLOCKED", error),
        };
        let body = json!({ "query": query });
        let mut sent = self.es_post(&connection, "/_plugins/_ppl", &body).await;
        if let Ok(response) = &sent {
            if opensearch_query_endpoint_missing(response.status, &response.body) {
                sent = self.es_post(&connection, "/_opendistro/_ppl", &body).await;
            }
        }
        let response = match sent {
            Ok(response) => response,
            Err(error) => return request_error(error),
        };
        if !is_success(&response) {
            return tool_error("OPENSEARCH_PPL_ERROR", error_reason(response.status, &response.body));
        }
        let shaped = parse_body(&response).and_then(|body| shape_tabular_response(&body, limit));
        match shaped {
            Ok(mut shaped) => {
                shaped["query_executed"] = json!(query);
                shaped["time_window"] = match &window {
                    Some(window) => {
                        let (from, to) = window.ppl_bounds();
                        let mut described = window.describe(
                            &time_field,
                            optional_text(&request.from).unwrap_or(es::DEFAULT_WINDOW_FROM),
                            optional_text(&request.to).unwrap_or(es::DEFAULT_WINDOW_TO),
                        );
                        described["ppl_bounds"] = json!([from, to]);
                        described
                    }
                    None => json!("all time (no time filter)"),
                };
                if !notes.is_empty() {
                    shaped["notes"] = json!(notes);
                }
                json_result(shaped, "rows")
            }
            Err(error) => request_error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(db_type: &str) -> ConnectionConfig {
        serde_json::from_value(json!({
            "id": "c", "name": "conn", "db_type": db_type, "host": "localhost", "port": 1,
            "username": "", "password": ""
        }))
        .unwrap()
    }

    #[test]
    fn only_search_connections_are_accepted() {
        assert!(ensure_search_connection(&connection("elasticsearch")).is_ok());
        assert!(ensure_search_connection(&connection("easysearch")).is_ok());
        let error = ensure_search_connection(&connection("postgres")).unwrap_err();
        assert!(error.contains("postgres connection") && error.contains("dbx_execute_query"), "{error}");
        assert!(ensure_search_connection(&connection("redis")).unwrap_err().contains("dbx_execute_redis_command"));
    }

    #[test]
    fn notes_missing_or_non_date_time_fields() {
        let field = |name: &str, kind: &str| FieldCapability {
            name: name.into(),
            field_type: kind.into(),
            types: vec![kind.into()],
            searchable: true,
            aggregatable: true,
        };
        let fields = vec![field("event.created", "date"), field("ts", "keyword")];
        assert!(time_field_note(&fields, "@timestamp").unwrap().contains("event.created"));
        assert!(time_field_note(&fields, "ts").unwrap().contains("not date"));
        assert!(time_field_note(&fields, "event.created").is_none());
        assert_eq!(OPENSEARCH_TOOL_NAMES.len(), 6);
    }
}
