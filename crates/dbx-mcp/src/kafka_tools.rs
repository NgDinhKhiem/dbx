//! Read-only Kafka observability tools (`dbx_kafka_*`).
//!
//! Declared as a child module of `server` so it can reuse the server's policy
//! helpers (tool allow-list, connection/group scope). Data comes from the
//! backend's single `kafka_observe` call; shaping lives in
//! `dbx_core::mq::kafka_observability`.

use std::time::Duration;

use dbx_core::models::connection::ConnectionConfig;
use dbx_core::mq::kafka_observability::{self as kafka, KafkaObserveRequest};
use dbx_core::mq::{ClusterInfo, KafkaConsumerGroupSnapshot, TopicInfo, TopicStats};
use rmcp::{handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router};
use serde::Deserialize;

use super::{backend_tool_error, text, tool_error, ConnectionSelector, DbxMcpServer};

/// Every tool this module registers, in registration order.
#[cfg(test)]
pub(crate) const KAFKA_TOOL_NAMES: &[&str] = &[
    "dbx_kafka_cluster",
    "dbx_kafka_list_topics",
    "dbx_kafka_describe_topic",
    "dbx_kafka_topic_config",
    "dbx_kafka_list_consumer_groups",
    "dbx_kafka_consumer_group",
    "dbx_kafka_lag",
    "dbx_kafka_throughput",
];

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KafkaConnectionRequest {
    #[serde(flatten)]
    pub selector: ConnectionSelector,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KafkaTopicRequest {
    #[serde(flatten)]
    pub selector: ConnectionSelector,
    #[schemars(description = "Kafka topic name")]
    pub topic: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KafkaGroupRequest {
    #[serde(flatten)]
    pub selector: ConnectionSelector,
    #[schemars(description = "Consumer group id")]
    pub group: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KafkaLagRequest {
    #[serde(flatten)]
    pub selector: ConnectionSelector,
    #[schemars(description = "Only this consumer group (optional)", extend("type" = "string"))]
    pub group: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KafkaThroughputRequest {
    #[serde(flatten)]
    pub selector: ConnectionSelector,
    #[schemars(description = "Only this consumer group (optional)", extend("type" = "string"))]
    pub group: Option<String>,
    #[schemars(
        description = "Seconds between the two readings, clamped to 2-60 (default 10). Consumers commit every 5s by default, so short windows can misreport a healthy consumer as stalled.",
        extend("type" = "integer")
    )]
    pub sample_seconds: Option<u64>,
}

fn optional_group(group: Option<String>) -> Option<String> {
    group.map(|group| group.trim().to_string()).filter(|group| !group.is_empty())
}

fn json_text(value: serde_json::Value) -> CallToolResult {
    text(value.to_string())
}

impl DbxMcpServer {
    /// Tool allow-list, connection/group scope, then the Kafka-only check.
    #[allow(clippy::result_large_err)]
    async fn kafka_connection(
        &self,
        tool_name: &str,
        selector: &ConnectionSelector,
    ) -> Result<ConnectionConfig, CallToolResult> {
        self.ensure_tool_allowed(tool_name).await?;
        let resolved = self.resolve_connection(selector).await?;
        kafka::ensure_kafka_connection(&resolved.connection)
            .map_err(|error| tool_error("KAFKA_CONNECTION_REQUIRED", error))?;
        Ok(resolved.connection)
    }

    async fn kafka_fetch<T: serde::de::DeserializeOwned>(
        &self,
        connection: &ConnectionConfig,
        request: KafkaObserveRequest,
        what: &str,
    ) -> Result<T, String> {
        let value = self.backend.kafka_observe(connection, request).await?;
        kafka::decode(value, what)
    }

    async fn kafka_snapshot(&self, connection: &ConnectionConfig) -> Result<KafkaConsumerGroupSnapshot, String> {
        self.kafka_fetch(connection, KafkaObserveRequest::ConsumerGroups, "consumer group").await
    }
}

fn request_error(error: String) -> CallToolResult {
    backend_tool_error("KAFKA_REQUEST_ERROR", error)
}

#[tool_router(router = kafka_tool_router, vis = "pub(crate)")]
impl DbxMcpServer {
    #[tool(name = "dbx_kafka_cluster", description = "Kafka cluster metadata: controller, brokers, topic count.")]
    async fn kafka_cluster(&self, Parameters(request): Parameters<KafkaConnectionRequest>) -> CallToolResult {
        let connection = match self.kafka_connection("dbx_kafka_cluster", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        let cluster: ClusterInfo = match self.kafka_fetch(&connection, KafkaObserveRequest::Cluster, "cluster").await {
            Ok(cluster) => cluster,
            Err(error) => return request_error(error),
        };
        let topics = self.kafka_fetch::<Vec<TopicInfo>>(&connection, KafkaObserveRequest::Topics, "topic list").await;
        json_text(kafka::shape_cluster(&cluster, topics.as_deref().map_err(String::as_str)))
    }

    #[tool(
        name = "dbx_kafka_list_topics",
        description = "List Kafka topics with partition and replica counts, sorted by name (internal topics last), with totals."
    )]
    async fn kafka_list_topics(&self, Parameters(request): Parameters<KafkaConnectionRequest>) -> CallToolResult {
        let connection = match self.kafka_connection("dbx_kafka_list_topics", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        match self.kafka_fetch::<Vec<TopicInfo>>(&connection, KafkaObserveRequest::Topics, "topic list").await {
            Ok(topics) => json_text(kafka::shape_topics(&topics, kafka::MAX_TOPIC_ROWS)),
            Err(error) => request_error(error),
        }
    }

    #[tool(
        name = "dbx_kafka_describe_topic",
        description = "One Kafka topic per partition: leader, replicas, in-sync replicas, start and end offsets, and message count, plus under-replicated and offline partitions."
    )]
    async fn kafka_describe_topic(&self, Parameters(request): Parameters<KafkaTopicRequest>) -> CallToolResult {
        let connection = match self.kafka_connection("dbx_kafka_describe_topic", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        let topic = request.topic.trim().to_string();
        if topic.is_empty() {
            return tool_error("KAFKA_TOPIC_REQUIRED", "topic must not be empty.");
        }
        let stats: TopicStats = match self
            .kafka_fetch(&connection, KafkaObserveRequest::TopicStats { topic: topic.clone() }, "topic stats")
            .await
        {
            Ok(stats) => stats,
            Err(error) => return request_error(error),
        };
        json_text(kafka::shape_topic(&topic, &stats, kafka::MAX_PARTITION_ROWS))
    }

    #[tool(
        name = "dbx_kafka_topic_config",
        description = "Configuration of one Kafka topic: key settings (retention, cleanup policy, min ISR, sizes) with readable values, topic-level overrides, and defaults. Sensitive values are redacted."
    )]
    async fn kafka_topic_config(&self, Parameters(request): Parameters<KafkaTopicRequest>) -> CallToolResult {
        let connection = match self.kafka_connection("dbx_kafka_topic_config", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        let topic = request.topic.trim().to_string();
        if topic.is_empty() {
            return tool_error("KAFKA_TOPIC_REQUIRED", "topic must not be empty.");
        }
        match self
            .kafka_fetch::<serde_json::Value>(
                &connection,
                KafkaObserveRequest::TopicConfig { topic: topic.clone() },
                "topic config",
            )
            .await
        {
            Ok(raw) => json_text(kafka::shape_topic_config(&topic, &raw)),
            Err(error) => request_error(error),
        }
    }

    #[tool(
        name = "dbx_kafka_list_consumer_groups",
        description = "List Kafka consumer groups with state, member count, topics and total lag, with totals."
    )]
    async fn kafka_list_consumer_groups(
        &self,
        Parameters(request): Parameters<KafkaConnectionRequest>,
    ) -> CallToolResult {
        let connection = match self.kafka_connection("dbx_kafka_list_consumer_groups", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        match self.kafka_snapshot(&connection).await {
            Ok(snapshot) => json_text(kafka::shape_consumer_groups(&snapshot, kafka::MAX_GROUP_ROWS)),
            Err(error) => request_error(error),
        }
    }

    #[tool(
        name = "dbx_kafka_consumer_group",
        description = "One Kafka consumer group in detail: committed offset, end offset and lag for each partition it reads, plus per-topic totals."
    )]
    async fn kafka_consumer_group(&self, Parameters(request): Parameters<KafkaGroupRequest>) -> CallToolResult {
        let connection = match self.kafka_connection("dbx_kafka_consumer_group", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        let group = request.group.trim();
        if group.is_empty() {
            return tool_error("KAFKA_GROUP_REQUIRED", "group must not be empty.");
        }
        let snapshot = match self.kafka_snapshot(&connection).await {
            Ok(snapshot) => snapshot,
            Err(error) => return request_error(error),
        };
        match kafka::find_group(&snapshot, group) {
            Ok(summary) => json_text(kafka::shape_consumer_group(summary, kafka::MAX_PARTITION_ROWS)),
            Err(error) => tool_error("KAFKA_GROUP_NOT_FOUND", error),
        }
    }

    #[tool(
        name = "dbx_kafka_lag",
        description = "Kafka consumer lag for every group and topic, worst first. max_partition_lag separates a slow consumer (lag spread evenly across partitions) from a stuck one (lag piled on one partition)."
    )]
    async fn kafka_lag(&self, Parameters(request): Parameters<KafkaLagRequest>) -> CallToolResult {
        let connection = match self.kafka_connection("dbx_kafka_lag", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        let group = optional_group(request.group);
        let snapshot = match self.kafka_snapshot(&connection).await {
            Ok(snapshot) => snapshot,
            Err(error) => return request_error(error),
        };
        if let Some(group) = group.as_deref() {
            if let Err(error) = kafka::find_group(&snapshot, group) {
                return tool_error("KAFKA_GROUP_NOT_FOUND", error);
            }
        }
        json_text(kafka::shape_lag(&snapshot, group.as_deref(), kafka::MAX_LAG_ROWS))
    }

    #[tool(
        name = "dbx_kafka_throughput",
        description = "Kafka processing speed: reads end offsets and committed offsets twice, sample_seconds apart (default 10, clamped 2-60), and reports produced and consumed messages per second, how fast the backlog is changing, estimated time to catch up, and a status (caught up / stalled / falling behind / catching up / keeping pace). Takes sample_seconds to answer."
    )]
    async fn kafka_throughput(&self, Parameters(request): Parameters<KafkaThroughputRequest>) -> CallToolResult {
        let connection = match self.kafka_connection("dbx_kafka_throughput", &request.selector).await {
            Ok(connection) => connection,
            Err(error) => return error,
        };
        let group = optional_group(request.group);
        let seconds = kafka::clamp_sample_seconds(request.sample_seconds);
        let clock = kafka::TokioClock::default();
        let samples =
            kafka::sample_twice(&clock, Duration::from_secs(seconds), || self.kafka_snapshot(&connection)).await;
        let (first, second) = match samples {
            Ok(samples) => samples,
            Err(error) => return request_error(error),
        };
        if let Some(group) = group.as_deref() {
            if let Err(error) = kafka::find_group(&second.snapshot, group) {
                return tool_error("KAFKA_GROUP_NOT_FOUND", error);
            }
        }
        json_text(kafka::shape_throughput(&first, &second, group.as_deref(), kafka::MAX_LAG_ROWS))
    }
}
