//! `dbx_opensearch_*` tools end to end: MCP client -> server -> Local / Web
//! backend -> a mock OpenSearch cluster (or mock DBX Web API).

use std::sync::{Arc, Mutex};

use dbx_core::{
    models::connection::ConnectionConfig,
    storage::{McpGlobalPolicy, Storage},
};
use dbx_mcp::{DbxMcpServer, LocalBackend, McpScope, WebBackend};
use rmcp::{model::CallToolRequestParams, service::RunningService, RoleClient, ServiceExt};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    body: String,
}

type Handler = Arc<dyn Fn(&str, &str, &str) -> (u16, String) + Send + Sync>;

/// Minimal HTTP/1.1 server: one request per connection, answered by `handler`.
struct MockHttp {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockHttp {
    async fn start(handler: Handler) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let handler = handler.clone();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 8192];
                    let header_end = loop {
                        let read = socket.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            return; // bare reachability probe
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                            break index + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    while buffer.len() < header_end + length {
                        let read = socket.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                    let body = String::from_utf8_lossy(&buffer[header_end..]).to_string();
                    let mut request_line = head.lines().next().unwrap_or_default().split_whitespace();
                    let method = request_line.next().unwrap_or_default().to_string();
                    let path = request_line.next().unwrap_or_default().to_string();
                    let (status, response) = handler(&method, &path, &body);
                    recorded.lock().unwrap().push(Recorded { method, path, body });
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        Self { port, requests, task }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn requests_to(&self, prefix: &str) -> Vec<Recorded> {
        self.requests().into_iter().filter(|request| request.path.starts_with(prefix)).collect()
    }
}

impl Drop for MockHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

const ROOT: &str =
    r#"{"name":"node","version":{"distribution":"opensearch","number":"1.3.19"},"tagline":"The OpenSearch Project"}"#;

fn opensearch_cluster(method: &str, path: &str, body: &str) -> (u16, String) {
    let route = path.split('?').next().unwrap_or(path);
    match (method, route) {
        ("GET", "/") => (200, ROOT.into()),
        ("POST", "/.kibana,.kibana_*/_search") => (
            200,
            json!({ "hits": { "hits": [
                { "_index": ".kibana_1", "_id": "index-pattern:p1", "_source": { "type": "index-pattern", "index-pattern": { "title": "logs-*", "timeFieldName": "@timestamp" } } },
                { "_index": ".kibana_1", "_id": "index-pattern:p2", "_source": { "type": "index-pattern", "index-pattern": { "title": "orders" } } },
                { "_index": ".kibana_1", "_id": "index-pattern:p3", "_source": { "type": "index-pattern", "index-pattern": { "title": "audit-*", "timeFieldName": "event.created" } } }
            ] } })
            .to_string(),
        ),
        ("GET", "/_cat/indices") => (
            200,
            json!([
                { "index": "logs-2026.09.26", "health": "green", "status": "open", "docs.count": "120", "store.size": "1mb" },
                { "index": "logs-2026.09.25", "health": "green", "status": "open", "docs.count": "80", "store.size": "1mb" },
                { "index": "orders", "health": "yellow", "status": "open", "docs.count": "5", "store.size": "10kb" },
                { "index": ".kibana_1", "health": "green", "status": "open", "docs.count": "3", "store.size": "5kb" }
            ])
            .to_string(),
        ),
        ("GET", "/_resolve/index/logs-*") => (
            200,
            json!({ "indices": [
                { "name": "logs-2026.09.25", "attributes": ["open"] },
                { "name": "logs-2026.09.26", "aliases": ["logs"], "attributes": ["open"] }
            ], "aliases": [{ "name": "logs", "indices": ["logs-2026.09.26"] }], "data_streams": [] })
            .to_string(),
        ),
        ("GET", "/logs-*/_field_caps") | ("GET", "/audit-*/_field_caps") => (
            200,
            json!({ "indices": ["logs-2026.09.25", "logs-2026.09.26"], "fields": {
                "@timestamp": { "date": { "type": "date", "searchable": true, "aggregatable": true } },
                "level": { "keyword": { "type": "keyword", "searchable": true, "aggregatable": true } },
                "kubernetes.pod": { "keyword": { "type": "keyword", "searchable": true, "aggregatable": true } },
                "kubernetes.ns": { "keyword": { "type": "keyword", "searchable": true, "aggregatable": true } },
                "payload": { "keyword": { "type": "keyword", "searchable": false, "aggregatable": false } }
            } })
            .to_string(),
        ),
        ("POST", "/logs-*/_search") | ("POST", "/orders/_search") | ("POST", "/audit-*/_search") => (
            200,
            json!({ "took": 3, "hits": { "total": { "value": 2, "relation": "eq" }, "hits": [
                { "_index": "logs-2026.09.26", "_id": "a", "_source": { "@timestamp": "2026-09-26T10:00:00Z", "level": "ERROR", "message": "x".repeat(3000) } },
                { "_index": "logs-2026.09.26", "_id": "b", "_source": { "@timestamp": "2026-09-26T09:59:00Z", "level": "ERROR" } }
            ] } })
            .to_string(),
        ),
        ("POST", "/_plugins/_sql") => {
            (400, r#"{"error":"no handler found for uri [/_plugins/_sql] and method [POST]"}"#.into())
        }
        ("POST", "/_opendistro/_sql") => (
            200,
            json!({ "schema": [{ "name": "level", "type": "keyword" }, { "name": "count(*)", "type": "integer" }], "datarows": [["ERROR", 7], ["WARN", 3]], "total": 2, "size": 2, "status": 200 })
                .to_string(),
        ),
        ("POST", "/_plugins/_ppl") => {
            let query = serde_json::from_str::<Value>(body).unwrap()["query"].as_str().unwrap_or_default().to_string();
            if query.contains("where") {
                (200, json!({ "schema": [{ "name": "count()", "type": "integer" }], "datarows": [[42]], "total": 1, "size": 1 }).to_string())
            } else {
                (400, r#"{"error":{"reason":"missing window","details":"x","type":"x"},"status":400}"#.into())
            }
        }
        _ => (404, format!(r#"{{"error":"unexpected {method} {path}"}}"#)),
    }
}

fn es_connection(port: u16) -> ConnectionConfig {
    serde_json::from_value(json!({
        "id": "os", "name": "logs-os", "db_type": "elasticsearch", "host": "127.0.0.1", "port": port,
        "username": "", "password": "", "ssl": false, "read_only": true
    }))
    .unwrap()
}

fn postgres_connection() -> ConnectionConfig {
    serde_json::from_value(json!({
        "id": "pg", "name": "orders-pg", "db_type": "postgres", "host": "127.0.0.1", "port": 5432,
        "username": "u", "password": "", "database": "app", "ssl": false
    }))
    .unwrap()
}

struct Harness {
    client: RunningService<RoleClient, ()>,
    server_task: tokio::task::JoinHandle<()>,
    _directory: TempDir,
}

impl Harness {
    async fn local(connections: Vec<ConnectionConfig>, policy: Option<McpGlobalPolicy>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("dbx.db");
        let storage = Storage::open(&db_path).await.unwrap();
        storage.save_connections(&connections).await.unwrap();
        if let Some(policy) = policy {
            storage.save_mcp_global_policy(&policy).await.unwrap();
        }
        let backend = Arc::new(LocalBackend::open(&db_path).await.unwrap());
        Self::serve(DbxMcpServer::with_runtime_options(backend, McpScope::default(), false), directory).await
    }

    async fn serve(server: DbxMcpServer, directory: TempDir) -> Self {
        let (server_transport, client_transport) = tokio::io::duplex(1024 * 1024);
        let server_task = tokio::spawn(async move {
            if let Ok(service) = server.serve(server_transport).await {
                let _ = service.waiting().await;
            }
        });
        let client = ().serve(client_transport).await.unwrap();
        Self { client, server_task, _directory: directory }
    }

    async fn call(&self, tool: &str, arguments: Value) -> (bool, String) {
        let result = self
            .client
            .peer()
            .call_tool(
                CallToolRequestParams::new(tool.to_string()).with_arguments(arguments.as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        (result.is_error == Some(true), result.content[0].as_text().unwrap().text.clone())
    }

    async fn call_json(&self, tool: &str, arguments: Value) -> Value {
        let (is_error, text) = self.call(tool, arguments).await;
        assert!(!is_error, "{tool} failed: {text}");
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{tool} returned invalid JSON ({error}): {text}"))
    }

    async fn close(self) {
        let _ = self.client.cancel().await;
        self.server_task.abort();
    }
}

fn parse_time(value: &Value) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(value.as_str().unwrap()).unwrap().with_timezone(&chrono::Utc)
}

#[tokio::test]
async fn opensearch_tools_are_listed() {
    let harness = Harness::local(vec![], None).await;
    let tools = harness.client.peer().list_tools(None).await.unwrap();
    let names: Vec<&str> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    for name in [
        "dbx_opensearch_list_indices",
        "dbx_opensearch_resolve_pattern",
        "dbx_opensearch_fields",
        "dbx_opensearch_search",
        "dbx_opensearch_sql",
        "dbx_opensearch_ppl",
    ] {
        assert!(names.contains(&name), "{name} missing from {names:?}");
    }
    let search = tools.tools.iter().find(|tool| tool.name == "dbx_opensearch_search").unwrap();
    let properties = search.input_schema.get("properties").unwrap();
    assert_eq!(properties["language"]["enum"], json!(["dql", "lucene"]));
    let sql = tools.tools.iter().find(|tool| tool.name == "dbx_opensearch_sql").unwrap();
    let description = sql.description.as_deref().unwrap();
    assert!(description.contains("backticks") && description.contains("NOW()"), "{description}");
    harness.close().await;
}

#[tokio::test]
async fn search_applies_the_default_window_dql_and_compact_hits() {
    let cluster = MockHttp::start(Arc::new(opensearch_cluster)).await;
    let harness = Harness::local(vec![es_connection(cluster.port)], None).await;

    let result = harness
        .call_json(
            "dbx_opensearch_search",
            json!({ "connection_id": "os", "pattern": "logs-*", "query": "level:ERROR and kubernetes.*:web", "limit": 500 }),
        )
        .await;
    let window = &result["time_window"];
    assert_eq!(window["field"], "@timestamp");
    assert_eq!((window["from_expression"].as_str(), window["to_expression"].as_str()), (Some("now-1h"), Some("now")));
    assert_eq!(parse_time(&window["to"]) - parse_time(&window["from"]), chrono::Duration::hours(1));
    assert_eq!(result["total"], json!({ "value": 2, "relation": "eq" }));
    assert_eq!(result["returned"], 2);
    let hit = &result["hits"][0];
    assert_eq!((hit["_index"].as_str(), hit["_id"].as_str()), (Some("logs-2026.09.26"), Some("a")));
    assert_eq!(hit["time"], "2026-09-26T10:00:00Z");
    assert!(hit["_source"]["message"].as_str().unwrap().contains("[truncated 1000 more chars]"));

    let searches = cluster.requests_to("/logs-*/_search");
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0].method, "POST");
    let body: Value = serde_json::from_str(&searches[0].body).unwrap();
    assert_eq!(body["size"], 200, "limit is capped at 200");
    assert_eq!(
        body["query"]["bool"]["must"][0],
        json!({ "bool": { "filter": [
            { "match": { "level": "ERROR" } },
            { "bool": { "should": [{ "match": { "kubernetes.ns": "web" } }, { "match": { "kubernetes.pod": "web" } }], "minimum_should_match": 1 } }
        ] } })
    );
    let range = &body["query"]["bool"]["filter"][0]["range"]["@timestamp"];
    assert_eq!(range["gte"], window["from"]);
    assert_eq!(range["lte"], window["to"]);
    assert_eq!(body["sort"], json!([{ "@timestamp": { "order": "desc", "unmapped_type": "boolean" } }]));
    // Wildcard field names were expanded from _field_caps for just that pattern.
    let caps = cluster.requests_to("/logs-*/_field_caps");
    assert!(caps[0].path.contains("fields=kubernetes.*"), "{:?}", caps[0].path);
    harness.close().await;
}

#[tokio::test]
async fn search_uses_saved_pattern_time_fields_lucene_absolute_windows_and_all_time() {
    let cluster = MockHttp::start(Arc::new(opensearch_cluster)).await;
    let harness = Harness::local(vec![es_connection(cluster.port)], None).await;

    let result = harness
        .call_json(
            "dbx_opensearch_search",
            json!({ "connection_name": "logs-os", "pattern": "audit-*", "query": "user:bob AND action:del*", "language": "lucene", "from": "2026-09-26 08:00:00", "to": "2026-09-26T09:00:00+01:00" }),
        )
        .await;
    assert_eq!(result["time_window"]["field"], "event.created");
    assert_eq!(result["time_window"]["from"], "2026-09-26T08:00:00.000Z");
    assert_eq!(result["time_window"]["to"], "2026-09-26T08:00:00.000Z");
    let body: Value = serde_json::from_str(&cluster.requests_to("/audit-*/_search")[0].body).unwrap();
    assert_eq!(
        body["query"]["bool"]["must"][0],
        json!({ "query_string": { "query": "user:bob AND action:del*", "analyze_wildcard": true } })
    );
    assert!(body["query"]["bool"]["filter"][0]["range"]["event.created"].is_object());

    // A saved pattern without a time field gets no window.
    let result =
        harness.call_json("dbx_opensearch_search", json!({ "connection_id": "os", "pattern": "orders" })).await;
    assert_eq!(result["time_window"], "all time (no time filter)");
    assert!(result["notes"][0].as_str().unwrap().contains("no time field"));
    let body: Value = serde_json::from_str(&cluster.requests_to("/orders/_search")[0].body).unwrap();
    assert_eq!(body["query"], json!({ "match_all": {} }));
    assert!(body.get("sort").is_none());

    let result = harness
        .call_json(
            "dbx_opensearch_search",
            json!({ "connection_id": "os", "pattern": "logs-*", "all_time": true, "from": "now-5m" }),
        )
        .await;
    assert_eq!(result["time_window"], "all time (no time filter)");
    assert!(result["notes"][0].as_str().unwrap().contains("ignored"));

    // Saved patterns are read once and cached per connection.
    assert_eq!(cluster.requests_to("/.kibana").len(), 1);
    harness.close().await;
}

#[tokio::test]
async fn search_rejects_bad_dql_and_time_before_querying() {
    let cluster = MockHttp::start(Arc::new(opensearch_cluster)).await;
    let harness = Harness::local(vec![es_connection(cluster.port)], None).await;
    let (is_error, text) = harness
        .call(
            "dbx_opensearch_search",
            json!({ "connection_id": "os", "pattern": "logs-*", "query": "message:\"abc", "time_field": "@timestamp" }),
        )
        .await;
    assert!(is_error);
    assert!(text.contains("DQL_SYNTAX_ERROR") && text.contains("Unterminated quoted string at position 9"), "{text}");
    assert!(text.ends_with("message:\"abc\n        ^"), "{text}");
    let (is_error, text) = harness
        .call(
            "dbx_opensearch_search",
            json!({ "connection_id": "os", "pattern": "logs-*", "from": "yesterday", "time_field": "@timestamp" }),
        )
        .await;
    assert!(is_error && text.contains("INVALID_TIME_RANGE") && text.contains("now-15m"), "{text}");
    assert!(cluster.requests_to("/logs-*/_search").is_empty());
    harness.close().await;
}

#[tokio::test]
async fn list_indices_resolve_and_fields() {
    let cluster = MockHttp::start(Arc::new(opensearch_cluster)).await;
    let harness = Harness::local(vec![es_connection(cluster.port)], None).await;

    let listed = harness.call_json("dbx_opensearch_list_indices", json!({ "connection_id": "os" })).await;
    let titles: Vec<&str> =
        listed["index_patterns"].as_array().unwrap().iter().map(|p| p["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["audit-*", "logs-*", "orders"]);
    assert_eq!(listed["index_patterns"][1]["time_field"], "@timestamp");
    assert_eq!(listed["suggested_patterns"], json!([{ "pattern": "logs-*", "indices": 2 }]));
    assert_eq!(listed["index_count"], 3);
    assert_eq!(listed["hidden_indices_omitted"], 1);
    let keys: Vec<&String> = listed.as_object().unwrap().keys().collect();
    assert!(keys.iter().position(|key| *key == "index_patterns") < keys.iter().position(|key| *key == "indices"));

    let resolved = harness
        .call_json("dbx_opensearch_resolve_pattern", json!({ "connection_id": "os", "pattern": "logs-*" }))
        .await;
    assert_eq!(resolved["index_count"], 2);
    assert_eq!(resolved["aliases"][0]["name"], "logs");
    assert_eq!(resolved["saved_index_pattern"]["time_field"], "@timestamp");

    let fields =
        harness.call_json("dbx_opensearch_fields", json!({ "connection_id": "os", "pattern": "logs-*" })).await;
    assert_eq!(fields["default_time_field"], "@timestamp");
    assert_eq!(fields["date_fields"], json!(["@timestamp"]));
    assert_eq!(fields["not_searchable"], json!(["payload"]));
    assert_eq!(fields["field_count"], 5);
    assert!(fields["hint"].as_str().unwrap().contains("searchable"));
    assert!(cluster.requests_to("/logs-*/_field_caps")[0].path.contains("fields=*"));

    // Every request was a read.
    for request in cluster.requests() {
        assert!(request.method == "GET" || request.path.contains("_search"), "{request:?}");
    }
    harness.close().await;
}

#[tokio::test]
async fn sql_falls_back_to_opendistro_and_rejects_writes() {
    let cluster = MockHttp::start(Arc::new(opensearch_cluster)).await;
    let harness = Harness::local(vec![es_connection(cluster.port)], None).await;

    let result = harness
        .call_json("dbx_opensearch_sql", json!({ "connection_id": "os", "query": "SELECT level, count(*) FROM `logs-*` GROUP BY level;", "limit": 1 }))
        .await;
    assert_eq!(result["engine"], "opensearch");
    assert_eq!(result["rows"], json!([["ERROR", 7]]));
    assert_eq!(result["fetched"], 2);
    assert!(result["notes"][0].as_str().unwrap().contains("first 1 of 2"));
    let paths: Vec<String> =
        cluster.requests().into_iter().filter(|r| r.path.contains("_sql")).map(|r| r.path).collect();
    assert_eq!(paths, vec!["/_plugins/_sql?format=jdbc", "/_opendistro/_sql?format=jdbc"]);
    let sent: Value = serde_json::from_str(&cluster.requests_to("/_opendistro/_sql")[0].body).unwrap();
    assert_eq!(sent["query"], "SELECT level, count(*) FROM `logs-*` GROUP BY level");

    let before = cluster.requests().len();
    for statement in ["DELETE FROM `logs-*`", "SELECT 1; DROP TABLE x"] {
        let (is_error, text) =
            harness.call("dbx_opensearch_sql", json!({ "connection_id": "os", "query": statement })).await;
        assert!(is_error && text.contains("SQL_BLOCKED"), "{statement}: {text}");
    }
    assert_eq!(cluster.requests().len(), before, "blocked SQL never reaches the cluster");
    harness.close().await;
}

#[tokio::test]
async fn ppl_inserts_the_window_after_source() {
    let cluster = MockHttp::start(Arc::new(opensearch_cluster)).await;
    let harness = Harness::local(vec![es_connection(cluster.port)], None).await;
    let result = harness
        .call_json(
            "dbx_opensearch_ppl",
            json!({ "connection_id": "os", "query": "source=logs-* | stats count()", "from": "2026-09-26T09:00:00Z", "to": "2026-09-26T10:00:00Z" }),
        )
        .await;
    let expected = "source=logs-* | where `@timestamp` >= '2026-09-26 09:00:00' and `@timestamp` <= '2026-09-26 10:00:00' | stats count()";
    assert_eq!(result["query_executed"], expected);
    assert_eq!(result["time_window"]["from"], "2026-09-26T09:00:00.000Z");
    assert_eq!(result["time_window"]["ppl_bounds"], json!(["2026-09-26 09:00:00", "2026-09-26 10:00:00"]));
    assert_eq!(result["rows"], json!([[42]]));
    let sent: Value = serde_json::from_str(&cluster.requests_to("/_plugins/_ppl")[0].body).unwrap();
    assert_eq!(sent["query"], expected);

    let (is_error, text) =
        harness.call("dbx_opensearch_ppl", json!({ "connection_id": "os", "query": "stats count()" })).await;
    assert!(is_error && text.contains("PPL_BLOCKED") && text.contains("source="), "{text}");
    harness.close().await;
}

#[tokio::test]
async fn non_search_connections_and_policy_are_enforced() {
    let cluster = MockHttp::start(Arc::new(opensearch_cluster)).await;
    let harness = Harness::local(vec![es_connection(cluster.port), postgres_connection()], None).await;
    let (is_error, text) =
        harness.call("dbx_opensearch_search", json!({ "connection_id": "pg", "pattern": "logs-*" })).await;
    assert!(is_error);
    assert!(text.contains("OPENSEARCH_CONNECTION_REQUIRED") && text.contains("dbx_execute_query"), "{text}");
    harness.close().await;

    // Connection allow-list.
    let policy = McpGlobalPolicy { allowed_connection_ids: Some(vec!["pg".into()]), ..Default::default() };
    let harness = Harness::local(vec![es_connection(cluster.port), postgres_connection()], Some(policy)).await;
    let (is_error, text) = harness.call("dbx_opensearch_list_indices", json!({ "connection_id": "os" })).await;
    assert!(is_error && text.contains("CONNECTION_OUT_OF_SCOPE"), "{text}");
    harness.close().await;

    // Tool allow-list: hidden from tools/list and refused when called.
    let policy = McpGlobalPolicy {
        allowed_tool_names: Some(vec!["dbx_list_connections".into(), "dbx_opensearch_fields".into()]),
        ..Default::default()
    };
    let harness = Harness::local(vec![es_connection(cluster.port)], Some(policy)).await;
    let tools = harness.client.peer().list_tools(None).await.unwrap();
    let names: Vec<&str> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert!(names.contains(&"dbx_opensearch_fields") && !names.contains(&"dbx_opensearch_search"), "{names:?}");
    let (is_error, text) =
        harness.call("dbx_opensearch_search", json!({ "connection_id": "os", "pattern": "logs-*" })).await;
    assert!(is_error && text.contains("TOOL_OUT_OF_SCOPE"), "{text}");
    harness.close().await;
    assert!(cluster.requests_to("/logs-*/_search").is_empty());
}

#[tokio::test]
async fn web_mode_routes_through_the_dbx_web_api() {
    let cluster_requests: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let seen = cluster_requests.clone();
    let port_cell: Arc<Mutex<u16>> = Arc::default();
    let port_for_handler = port_cell.clone();
    let web = MockHttp::start(Arc::new(move |method: &str, path: &str, body: &str| {
        let route = path.split('?').next().unwrap_or(path);
        match (method, route) {
            ("GET", "/api/auth/check") => {
                (200, r#"{"authenticated":true,"required":false,"setup_required":false}"#.into())
            }
            ("GET", "/api/app-settings/mcp-policy") => (
                200,
                r#"{"configured":true,"readOnly":true,"allowDangerousSql":false,"allowedConnectionIds":null}"#.into(),
            ),
            ("GET", "/api/connection/list") => {
                (200, serde_json::to_string(&vec![es_connection(*port_for_handler.lock().unwrap())]).unwrap())
            }
            ("POST", "/api/connection/connect") => (200, "{}".into()),
            ("POST", "/api/elasticsearch/cluster-info") => {
                (200, r#"{"distribution":"opensearch","version":"2.11.1"}"#.into())
            }
            ("POST", "/api/elasticsearch/raw-request") => {
                let request: Value = serde_json::from_str(body).unwrap();
                let (method, path) = (request["method"].as_str().unwrap(), request["path"].as_str().unwrap());
                seen.lock().unwrap().push((method.to_string(), path.to_string()));
                let inner_body = request["body"].as_str().unwrap_or_default();
                let (status, response) = opensearch_cluster(method, path, inner_body);
                (200, json!({ "status": status, "body": response, "tookMs": 1 }).to_string())
            }
            _ => (404, "{}".into()),
        }
    }))
    .await;
    *port_cell.lock().unwrap() = 9200;
    let backend = Arc::new(WebBackend::new(format!("http://127.0.0.1:{}", web.port), String::new()).unwrap());
    let harness = Harness::serve(
        DbxMcpServer::with_runtime_options(backend, McpScope::default(), true),
        tempfile::tempdir().unwrap(),
    )
    .await;
    let result = harness
        .call_json(
            "dbx_opensearch_search",
            json!({ "connection_id": "os", "pattern": "logs-*", "query": "level:ERROR", "from": "now-15m" }),
        )
        .await;
    assert_eq!(result["total"]["value"], 2);
    assert_eq!(
        parse_time(&result["time_window"]["to"]) - parse_time(&result["time_window"]["from"]),
        chrono::Duration::minutes(15)
    );
    let result = harness.call_json("dbx_opensearch_sql", json!({ "connection_id": "os", "query": "SELECT 1" })).await;
    assert_eq!(result["engine"], "opensearch");
    let requests = cluster_requests.lock().unwrap().clone();
    assert!(
        requests.iter().any(|(method, path)| method == "POST" && path.starts_with("/logs-*/_search")),
        "{requests:?}"
    );
    assert!(requests.iter().any(|(_, path)| path == "/_opendistro/_sql?format=jdbc"), "{requests:?}");
    harness.close().await;
}
