//! MCP-level tests for the read-only `dbx_kafka_*` tools against a fake Kafka backend.
#![cfg(feature = "mq-admin")]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dbx_core::{
    agent_events::ToolResult, agent_tools::AgentSqlPermissions, models::connection::ConnectionConfig,
    mq::kafka_observability::KafkaObserveRequest, storage::McpGlobalPolicy,
};
use dbx_mcp::{DbxBackend, DbxMcpServer, McpScope};
use rmcp::{model::CallToolRequestParams, service::RunningService, RoleClient, ServiceExt};
use serde_json::{json, Value};

const KAFKA_TOOLS: [&str; 8] = [
    "dbx_kafka_cluster",
    "dbx_kafka_list_topics",
    "dbx_kafka_describe_topic",
    "dbx_kafka_topic_config",
    "dbx_kafka_list_consumer_groups",
    "dbx_kafka_consumer_group",
    "dbx_kafka_lag",
    "dbx_kafka_throughput",
];

/// Answers `kafka_observe` like the Kafka agent would; each consumer group
/// snapshot call advances offsets so throughput sees movement.
struct FakeKafkaBackend {
    policy: McpGlobalPolicy,
    connections: Vec<ConnectionConfig>,
    calls: Mutex<Vec<(String, KafkaObserveRequest)>>,
}

impl FakeKafkaBackend {
    fn new(policy: McpGlobalPolicy, connections: Vec<ConnectionConfig>) -> Arc<Self> {
        Arc::new(Self { policy, connections, calls: Mutex::new(Vec::new()) })
    }

    fn ops(&self) -> Vec<KafkaObserveRequest> {
        self.calls.lock().unwrap().iter().map(|(_, request)| request.clone()).collect()
    }
}

fn snapshot(round: i64) -> Value {
    // "orders-app" consumes 20 msg/s behind a 10 msg/s producer (catching up);
    // "stuck-app" never commits (stalled, lag piled on partition 1).
    json!({"groups": [
        {"groupId": "orders-app", "state": "STABLE", "memberCount": 2, "topics": ["orders"],
         "totalLag": 1000 - 100 * round, "lagAvailable": true, "partitions": [
            {"topic": "orders", "partition": 0, "currentOffset": 100 * round, "endOffset": 500 + 50 * round, "lag": 500 - 50 * round},
            {"topic": "orders", "partition": 1, "currentOffset": 100 * round, "endOffset": 500 + 50 * round, "lag": 500 - 50 * round}
         ]},
        {"groupId": "stuck-app", "state": "STABLE", "memberCount": 1, "topics": ["orders"],
         "totalLag": 5000, "lagAvailable": true, "partitions": [
            {"topic": "orders", "partition": 0, "currentOffset": 500, "endOffset": 500, "lag": 0},
            {"topic": "orders", "partition": 1, "currentOffset": 0, "endOffset": 5000, "lag": 5000}
         ]}
    ]})
}

#[async_trait]
impl DbxBackend for FakeKafkaBackend {
    async fn kafka_observe(
        &self,
        connection: &ConnectionConfig,
        request: KafkaObserveRequest,
    ) -> Result<Value, String> {
        let round = {
            let mut calls = self.calls.lock().unwrap();
            calls.push((connection.id.clone(), request.clone()));
            calls.iter().filter(|(_, request)| *request == KafkaObserveRequest::ConsumerGroups).count() as i64 - 1
        };
        Ok(match request {
            KafkaObserveRequest::Cluster => json!({
                "clusterId": "c1", "brokerCount": 2, "controllerId": 1, "controllerHost": "b1:9092",
                "brokers": [{"id": 2, "host": "b2", "port": 9092, "rack": null}, {"id": 1, "host": "b1", "port": 9092, "rack": null}]
            }),
            KafkaObserveRequest::Topics => json!([
                {"name": "orders", "shortName": "orders", "partitioned": true, "partitions": 2, "replicationFactor": 2, "persistent": true, "internal": false},
                {"name": "__consumer_offsets", "shortName": "__consumer_offsets", "partitioned": true, "partitions": 50, "replicationFactor": 2, "persistent": true, "internal": true}
            ]),
            KafkaObserveRequest::TopicStats { topic } => json!({
                "msgRateIn": 0.0, "msgRateOut": 0.0, "msgThroughputIn": 0.0, "msgThroughputOut": 0.0, "storageSize": 0,
                "backlogSize": 0, "msgInCounter": 30, "msgOutCounter": 0, "subscriptionCount": 0, "producerCount": 0,
                "raw": {"name": topic, "partitions": 1, "replicationFactor": 2, "totalMessages": 30, "partitionStats": [
                    {"partition": 0, "leader": 1, "replicas": [1, 2], "isr": [1], "beginOffset": 70, "endOffset": 100, "messageCount": 30}
                ]}
            }),
            KafkaObserveRequest::TopicConfig { .. } => json!({"configs": {
                "cleanup.policy": {"value": "delete", "source": "DEFAULT_CONFIG", "isSensitive": false, "isReadOnly": false, "isDefault": true},
                "retention.ms": {"value": "3600000", "source": "DYNAMIC_TOPIC_CONFIG", "isSensitive": false, "isReadOnly": false, "isDefault": false}
            }}),
            KafkaObserveRequest::ConsumerGroups => snapshot(round),
        })
    }

    async fn load_mcp_global_policy(&self) -> Result<McpGlobalPolicy, String> {
        Ok(self.policy.clone())
    }

    async fn load_connections(&self) -> Result<Vec<ConnectionConfig>, String> {
        Ok(self.connections.clone())
    }

    async fn execute_agent_tool(
        &self,
        _connection: &ConnectionConfig,
        _database: &str,
        tool_name: &str,
        _arguments: Value,
        _permissions: AgentSqlPermissions,
    ) -> ToolResult {
        ToolResult {
            tool_call_id: "kafka-test".into(),
            tool_name: tool_name.into(),
            content: "not exercised".into(),
            is_error: true,
            explain_data: None,
        }
    }

    async fn add_connection_for_mcp(&self, config: ConnectionConfig) -> Result<ConnectionConfig, String> {
        Ok(config)
    }

    async fn duplicate_connection_for_mcp(&self, _: &str, _: &str, _: &str) -> Result<ConnectionConfig, String> {
        Err("not exercised".into())
    }

    async fn remove_connection_for_mcp(&self, _connection_id: &str) -> Result<bool, String> {
        Ok(true)
    }
}

fn connection(id: &str, name: &str, db_type: &str, external_config: Option<Value>) -> ConnectionConfig {
    let mut connection: ConnectionConfig = serde_json::from_value(json!({
        "id": id, "name": name, "db_type": db_type, "host": "", "port": 0,
        "username": "", "password": "", "database": "", "ssl": false
    }))
    .unwrap();
    connection.external_config = external_config;
    connection
}

fn kafka() -> ConnectionConfig {
    let mut kafka = connection(
        "kafka",
        "Kafka",
        "mq",
        Some(
            json!({"systemKind": "kafka", "adminUrl": "", "auth": {"kind": "none"}, "extra": {"bootstrapServers": "k:9092"}}),
        ),
    );
    // Observability is read-only: read-only and production protection must not block it.
    kafka.read_only = true;
    kafka.is_production = true;
    kafka
}

async fn start(backend: Arc<FakeKafkaBackend>) -> (RunningService<RoleClient, ()>, tokio::task::JoinHandle<()>) {
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server = DbxMcpServer::with_runtime_options(backend, McpScope::default(), false);
    let server_task = tokio::spawn(async move {
        if let Ok(running) = server.serve(server_transport).await {
            let _ = running.waiting().await;
        }
    });
    (().serve(client_transport).await.expect("initialize MCP client"), server_task)
}

async fn call(client: &RunningService<RoleClient, ()>, tool: &'static str, arguments: Value) -> (bool, String) {
    let result = client
        .peer()
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments.as_object().unwrap().clone()))
        .await
        .expect("call tool");
    (result.is_error == Some(true), result.content[0].as_text().expect("text").text.clone())
}

async fn call_json(client: &RunningService<RoleClient, ()>, tool: &'static str, arguments: Value) -> Value {
    let (is_error, text) = call(client, tool, arguments).await;
    assert!(!is_error, "{tool}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{tool} returned non-JSON ({error}): {text}"))
}

#[tokio::test]
async fn kafka_tools_are_listed_with_optional_single_typed_arguments() {
    let (client, server_task) = start(FakeKafkaBackend::new(McpGlobalPolicy::default(), vec![kafka()])).await;
    let tools = client.peer().list_tools(None).await.unwrap().tools;
    for name in KAFKA_TOOLS {
        let tool = tools.iter().find(|tool| tool.name == name).unwrap_or_else(|| panic!("{name} missing"));
        let properties = tool.input_schema.get("properties").and_then(Value::as_object).unwrap();
        assert!(properties.contains_key("connection_id") && properties.contains_key("connection_name"), "{name}");
        for optional in ["group", "sample_seconds"] {
            if let Some(schema) = properties.get(optional) {
                assert!(schema["type"].is_string(), "{name}.{optional}: {schema}");
            }
        }
    }
    let throughput = tools.iter().find(|tool| tool.name == "dbx_kafka_throughput").unwrap();
    let description = throughput.description.as_deref().unwrap();
    assert!(description.contains("Takes sample_seconds to answer"), "{description}");
    let required = throughput.input_schema.get("required").and_then(Value::as_array).cloned().unwrap_or_default();
    assert!(!required.iter().any(|field| field == "sample_seconds" || field == "group"));
    client.cancel().await.unwrap();
    server_task.abort();
}

#[tokio::test]
async fn kafka_tools_route_to_the_backend_and_shape_output() {
    let backend = FakeKafkaBackend::new(McpGlobalPolicy { read_only: true, ..Default::default() }, vec![kafka()]);
    let (client, server_task) = start(backend.clone()).await;
    let id = json!({"connection_id": "kafka"});

    let cluster = call_json(&client, "dbx_kafka_cluster", id.clone()).await;
    assert_eq!(cluster["controller"]["id"], 1);
    assert_eq!(cluster["brokers"][0]["address"], "b1:9092");
    assert_eq!(cluster["topic_count"], 2);

    let topics = call_json(&client, "dbx_kafka_list_topics", json!({"connection_name": "Kafka"})).await;
    assert_eq!(topics["topics"][0]["name"], "orders");
    assert_eq!(topics["topics"][0]["replication_factor"], 2);
    assert_eq!(topics["total_partitions"], 52);

    let topic =
        call_json(&client, "dbx_kafka_describe_topic", json!({"connection_id": "kafka", "topic": " orders "})).await;
    assert_eq!(topic["partitions"][0]["start_offset"], 70);
    assert_eq!(topic["under_replicated_partitions"], json!([0]));

    let config =
        call_json(&client, "dbx_kafka_topic_config", json!({"connection_id": "kafka", "topic": "orders"})).await;
    assert_eq!(config["key_settings"]["retention.ms"]["readable"], "1h 0m");
    assert_eq!(config["overrides"]["retention.ms"]["value"], "3600000");

    let groups = call_json(&client, "dbx_kafka_list_consumer_groups", id.clone()).await;
    assert_eq!(groups["group_count"], 2);
    assert_eq!(groups["total_lag"], 6000);

    let group =
        call_json(&client, "dbx_kafka_consumer_group", json!({"connection_id": "kafka", "group": "stuck-app"})).await;
    assert_eq!(group["partitions"][1]["lag"], 5000);
    let (missing, text) =
        call(&client, "dbx_kafka_consumer_group", json!({"connection_id": "kafka", "group": "nope"})).await;
    assert!(missing && text.contains("KAFKA_GROUP_NOT_FOUND") && text.contains("orders-app"), "{text}");

    let lag = call_json(&client, "dbx_kafka_lag", id.clone()).await;
    assert_eq!(lag["rows"][0]["group"], "stuck-app");
    assert_eq!(lag["rows"][0]["max_partition_lag"], 5000);
    assert_eq!(lag["rows"][0]["pattern"], "concentrated");
    assert_eq!(lag["rows"][1]["pattern"], "spread");
    let one = call_json(&client, "dbx_kafka_lag", json!({"connection_id": "kafka", "group": "orders-app"})).await;
    assert_eq!(one["rows"].as_array().unwrap().len(), 1);

    let ops = backend.ops();
    assert_eq!(ops[0], KafkaObserveRequest::Cluster);
    assert_eq!(ops[1], KafkaObserveRequest::Topics);
    assert!(ops.contains(&KafkaObserveRequest::TopicStats { topic: "orders".into() }), "topic is trimmed");
    assert!(ops.contains(&KafkaObserveRequest::TopicConfig { topic: "orders".into() }));
    assert!(backend.calls.lock().unwrap().iter().all(|(id, _)| id == "kafka"));
    client.cancel().await.unwrap();
    server_task.abort();
}

/// Paused tokio time: the 5 s sample window elapses instantly.
#[tokio::test(start_paused = true)]
async fn kafka_throughput_samples_twice_over_mcp() {
    let backend = FakeKafkaBackend::new(McpGlobalPolicy::default(), vec![kafka()]);
    let (client, server_task) = start(backend.clone()).await;
    let started = tokio::time::Instant::now();
    let body = call_json(&client, "dbx_kafka_throughput", json!({"connection_id": "kafka", "sample_seconds": 5})).await;
    assert!(started.elapsed() >= std::time::Duration::from_secs(5));
    assert_eq!(body["sample_seconds"], 5.0);
    assert_eq!(body["rows"][0]["group"], "stuck-app");
    assert_eq!(body["rows"][0]["status"], "stalled");
    let orders = body["rows"].as_array().unwrap().iter().find(|row| row["group"] == "orders-app").unwrap();
    assert_eq!(orders["status"], "catching up");
    assert_eq!(orders["produced_per_sec"], 20.0);
    assert_eq!(orders["consumed_per_sec"], 40.0);
    assert_eq!(orders["backlog_change_per_sec"], -20.0);
    assert_eq!(orders["time_to_catch_up"], "45s");
    assert_eq!(backend.ops(), vec![KafkaObserveRequest::ConsumerGroups, KafkaObserveRequest::ConsumerGroups]);
    client.cancel().await.unwrap();
    server_task.abort();
}

#[tokio::test]
async fn kafka_tools_refuse_non_kafka_connections_and_respect_policy() {
    let redis = connection("cache", "Cache", "redis", None);
    let rabbit = connection(
        "rabbit",
        "Rabbit",
        "mq",
        Some(json!({"systemKind": "rabbitmq", "adminUrl": "http://r:15672", "auth": {"kind": "none"}})),
    );
    let backend = FakeKafkaBackend::new(McpGlobalPolicy::default(), vec![kafka(), redis, rabbit]);
    let (client, server_task) = start(backend.clone()).await;
    for id in ["cache", "rabbit"] {
        for tool in KAFKA_TOOLS {
            let (is_error, text) =
                call(&client, tool, json!({"connection_id": id, "topic": "t", "group": "g", "sample_seconds": 2}))
                    .await;
            assert!(is_error && text.contains("KAFKA_CONNECTION_REQUIRED"), "{tool} on {id}: {text}");
            assert!(text.contains("dbx_list_connections") && text.contains("kafka"), "{text}");
        }
    }
    assert!(backend.ops().is_empty(), "refused calls must not reach the backend");
    client.cancel().await.unwrap();
    server_task.abort();

    for (policy, expected) in [
        (
            McpGlobalPolicy { allowed_tool_names: Some(vec!["dbx_kafka_lag".into()]), ..Default::default() },
            "TOOL_OUT_OF_SCOPE",
        ),
        (McpGlobalPolicy { allowed_connection_ids: Some(vec![]), ..Default::default() }, "CONNECTION_OUT_OF_SCOPE"),
    ] {
        let backend = FakeKafkaBackend::new(policy, vec![kafka()]);
        let (client, server_task) = start(backend.clone()).await;
        let (is_error, text) = call(&client, "dbx_kafka_cluster", json!({"connection_id": "kafka"})).await;
        assert!(is_error && text.contains(expected), "{text}");
        assert!(backend.ops().is_empty());
        client.cancel().await.unwrap();
        server_task.abort();
    }
}
