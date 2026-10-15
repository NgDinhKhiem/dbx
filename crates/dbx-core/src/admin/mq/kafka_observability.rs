//! Read-only Kafka observability used by the MCP tools (`dbx_kafka_*`).
//!
//! Everything here reuses the existing Kafka agent RPCs through the MQ admin
//! layer; nothing needs a new agent release:
//!
//! | need                                   | agent RPC (adapter method)                       |
//! |----------------------------------------|--------------------------------------------------|
//! | controller, brokers                    | `mq_describe_cluster` (`get_cluster_info`)       |
//! | topics, partitions, replication factor | `mq_list_topics` (`list_topics`)                 |
//! | per-partition leader/replicas/offsets  | `mq_get_topic_stats` (`get_topic_stats().raw`)   |
//! | topic configuration                    | `mq_get_topic_config` (`get_topic_internal_stats`)|
//! | groups, committed/end offsets, lag     | `mq_get_consumer_group_snapshot`                 |
//!
//! The module has two halves: [`kafka_observe_core`], the transport-neutral data
//! fetch that both the local MCP backend and the desktop bridge call, and pure
//! shaping functions that turn those payloads into compact, sorted, capped JSON
//! (lag aggregation, throughput math and status classification included).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::connection::AppState;
use crate::mq::config::MqAdminConfig;
use crate::mq::types::{
    ClusterInfo, KafkaConsumerGroupSnapshot, KafkaConsumerGroupSummary, ListTopicsOpts, MqSystemKind, NamespaceRef,
    TopicInfo, TopicRef, TopicStats,
};

/// Row caps that keep tool output model-sized. Totals are always computed over
/// every row; only the listed rows are truncated.
pub const MAX_TOPIC_ROWS: usize = 200;
pub const MAX_GROUP_ROWS: usize = 200;
pub const MAX_LAG_ROWS: usize = 200;
pub const MAX_PARTITION_ROWS: usize = 500;
/// Groups listed with their error in `dbx_kafka_lag` when offsets failed to load.
pub const MAX_GROUP_ERROR_ROWS: usize = 20;
pub const THROUGHPUT_DEFAULT_SECONDS: u64 = 10;
pub const THROUGHPUT_MIN_SECONDS: u64 = 2;
pub const THROUGHPUT_MAX_SECONDS: u64 = 60;

/// One read-only data fetch. Serialized as `{"op": "...", ...}` so it can cross
/// the desktop bridge as a single generic endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum KafkaObserveRequest {
    /// [`ClusterInfo`].
    Cluster,
    /// `Vec<TopicInfo>`.
    Topics,
    /// [`TopicStats`]; `raw.partitionStats` carries the per-partition rows.
    TopicStats { topic: String },
    /// `{ "configs": { name: { value, source, isDefault, isSensitive, isReadOnly } } }`.
    TopicConfig { topic: String },
    /// [`KafkaConsumerGroupSnapshot`].
    ConsumerGroups,
}

impl KafkaObserveRequest {
    /// The DBX Web API route (under `/api`) and body serving this request. Every
    /// route already exists for the MQ console and enforces the Web MCP scope.
    pub fn web_api_request(&self, connection_id: &str) -> (&'static str, Value) {
        match self {
            Self::Cluster => ("/api/mq/monitoring/cluster-info", json!({ "connectionId": connection_id })),
            Self::Topics => (
                "/api/mq/topics/list",
                json!({
                    "connectionId": connection_id,
                    "ns": { "tenant": "_kafka", "namespace": "default" },
                    "opts": ListTopicsOpts::default(),
                }),
            ),
            Self::TopicStats { topic } => {
                ("/api/mq/topics/stats", json!({ "connectionId": connection_id, "topic": kafka_topic_ref(topic) }))
            }
            Self::TopicConfig { topic } => (
                "/api/mq/topics/internal-stats",
                json!({ "connectionId": connection_id, "topic": kafka_topic_ref(topic) }),
            ),
            Self::ConsumerGroups => ("/api/mq/kafka/consumer-groups", json!({ "connectionId": connection_id })),
        }
    }
}

fn kafka_topic_ref(topic: &str) -> TopicRef {
    TopicRef {
        tenant: "_kafka".into(),
        namespace: "default".into(),
        topic: topic.trim().to_string(),
        persistent: true,
        ..Default::default()
    }
}

/// Refuse anything that is not a Kafka MQ connection, with a pointer to the right tools.
pub fn ensure_kafka_connection(connection: &crate::models::connection::ConnectionConfig) -> Result<(), String> {
    let kind = if connection.db_type == crate::models::connection::DatabaseType::MessageQueue {
        MqAdminConfig::from_connection(connection).ok().map(|config| config.system_kind)
    } else {
        None
    };
    if kind == Some(MqSystemKind::Kafka) {
        return Ok(());
    }
    let db_type = serde_json::to_value(connection.db_type)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{:?}", connection.db_type));
    let what = match kind {
        Some(kind) => format!("a {} message queue connection", kind.as_str()),
        None => format!("a {db_type} connection"),
    };
    Err(format!(
        "Connection \"{}\" is {what}, not Kafka. The dbx_kafka_* tools need a message queue connection \
         (db_type \"mq\") whose driver_profile/systemKind is \"kafka\"; use dbx_list_connections to find one. \
         For SQL use dbx_execute_query, for Redis dbx_execute_redis_command.",
        connection.name
    ))
}

/// Fetch one observability payload for a Kafka connection registered in `state.configs`.
pub async fn kafka_observe_core(
    state: &AppState,
    conn_id: &str,
    request: KafkaObserveRequest,
) -> Result<Value, String> {
    let config = state.configs.read().await.get(conn_id).cloned().ok_or("Connection not found")?;
    ensure_kafka_connection(&config)?;
    use crate::mq::service as svc;
    match request {
        KafkaObserveRequest::Cluster => to_value(svc::mq_get_cluster_info_core(state, conn_id).await),
        KafkaObserveRequest::Topics => {
            let ns = NamespaceRef { tenant: "_kafka".into(), namespace: "default".into() };
            to_value(svc::mq_list_topics_core(state, conn_id, ns, ListTopicsOpts::default()).await)
        }
        KafkaObserveRequest::TopicStats { topic } => {
            to_value(svc::mq_get_topic_stats_core(state, conn_id, kafka_topic_ref(&topic)).await)
        }
        KafkaObserveRequest::TopicConfig { topic } => {
            svc::mq_get_topic_internal_stats_core(state, conn_id, kafka_topic_ref(&topic)).await
        }
        KafkaObserveRequest::ConsumerGroups => {
            to_value(svc::mq_get_kafka_consumer_group_snapshot_core(state, conn_id).await)
        }
    }
}

fn to_value<T: Serialize>(value: Result<T, String>) -> Result<Value, String> {
    value.and_then(|value| serde_json::to_value(value).map_err(|error| error.to_string()))
}

/// Decode a payload returned by [`kafka_observe_core`] (possibly after a JSON round trip).
pub fn decode<T: serde::de::DeserializeOwned>(value: Value, what: &str) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| format!("Invalid Kafka {what} payload: {error}"))
}

// ---------------------------------------------------------------------------
// Shaping helpers
// ---------------------------------------------------------------------------

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// `3725` -> `"1h 2m"`, `42.4` -> `"42s"`.
pub fn human_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (days, hours, minutes, secs) = (total / 86_400, total % 86_400 / 3600, total % 3600 / 60, total % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

fn truncation_note(shown: usize, total: usize, what: &str) -> Option<String> {
    (shown < total).then(|| format!("Showing {shown} of {total} {what}; totals cover all {total}."))
}

fn put_note(output: &mut Value, note: Option<String>) {
    if let Some(note) = note {
        output["truncated"] = json!(true);
        output["note"] = json!(note);
    }
}

// ---- cluster / topics ----

pub fn shape_cluster(cluster: &ClusterInfo, topics: Result<&[TopicInfo], &str>) -> Value {
    let mut brokers = cluster.brokers.clone();
    brokers.sort_by_key(|broker| broker.id);
    let controller = match (cluster.controller_id, cluster.controller_host.as_deref()) {
        (None, None) => Value::Null,
        (id, host) => json!({ "id": id, "host": host }),
    };
    let mut output = json!({
        "cluster_id": cluster.cluster_id,
        "controller": controller,
        "broker_count": if cluster.broker_count > 0 { cluster.broker_count as usize } else { brokers.len() },
        "brokers": brokers.iter().map(|broker| {
            let mut row = json!({ "id": broker.id, "address": format!("{}:{}", broker.host, broker.port) });
            if let Some(rack) = broker.rack.as_deref() { row["rack"] = json!(rack); }
            row
        }).collect::<Vec<_>>(),
    });
    match topics {
        Ok(topics) => {
            let internal = topics.iter().filter(|topic| topic.internal).count();
            output["topic_count"] = json!(topics.len());
            output["internal_topic_count"] = json!(internal);
            output["total_partitions"] = json!(topics.iter().filter_map(|topic| topic.partitions).sum::<u32>());
        }
        Err(error) => {
            output["topic_count"] = Value::Null;
            output["topic_count_error"] = json!(error);
        }
    }
    output
}

pub fn shape_topics(topics: &[TopicInfo], limit: usize) -> Value {
    let mut sorted = topics.iter().collect::<Vec<_>>();
    sorted.sort_by(|a, b| a.internal.cmp(&b.internal).then_with(|| a.name.cmp(&b.name)));
    let rows = sorted
        .iter()
        .take(limit)
        .map(|topic| {
            let mut row = json!({
                "name": topic.name,
                "partitions": topic.partitions,
                "replication_factor": topic.replication_factor,
            });
            if topic.internal {
                row["internal"] = json!(true);
            }
            row
        })
        .collect::<Vec<_>>();
    let metadata_missing = topics.iter().filter(|topic| topic.partitions.is_none()).count();
    let mut output = json!({
        "topic_count": topics.len(),
        "internal_topic_count": topics.iter().filter(|topic| topic.internal).count(),
        "total_partitions": topics.iter().filter_map(|topic| topic.partitions).sum::<u32>(),
        "topics": rows,
    });
    if metadata_missing > 0 {
        output["metadata_note"] = json!(format!(
            "{metadata_missing} topic(s) have no partition metadata (the broker timed out or is too old); counts are null."
        ));
    }
    put_note(&mut output, truncation_note(limit.min(topics.len()), topics.len(), "topics"));
    output
}

fn i64_field(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn id_list(value: &Value, key: &str) -> Vec<i64> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

pub fn shape_topic(topic: &str, stats: &TopicStats, limit: usize) -> Value {
    let raw = &stats.raw;
    let mut partitions = raw.get("partitionStats").and_then(Value::as_array).cloned().unwrap_or_default();
    partitions.sort_by_key(|partition| i64_field(partition, "partition").unwrap_or(i64::MAX));
    let mut under_replicated = Vec::new();
    let mut offline = Vec::new();
    let rows = partitions
        .iter()
        .map(|partition| {
            let id = i64_field(partition, "partition");
            let replicas = id_list(partition, "replicas");
            let isr = id_list(partition, "isr");
            let leader = i64_field(partition, "leader");
            if leader.is_none_or(|leader| leader < 0) {
                offline.extend(id);
            }
            if isr.len() < replicas.len() {
                under_replicated.extend(id);
            }
            json!({
                "partition": id,
                "leader": leader.filter(|leader| *leader >= 0),
                "replicas": replicas,
                "isr": isr,
                "start_offset": i64_field(partition, "beginOffset"),
                "end_offset": i64_field(partition, "endOffset"),
                "messages": i64_field(partition, "messageCount"),
            })
        })
        .collect::<Vec<_>>();
    let total = rows.len();
    let mut output = json!({
        "topic": raw.get("name").and_then(Value::as_str).unwrap_or(topic),
        "partition_count": i64_field(raw, "partitions").unwrap_or(total as i64),
        "replication_factor": i64_field(raw, "replicationFactor"),
        "total_messages": i64_field(raw, "totalMessages").unwrap_or(stats.msg_in_counter),
        "under_replicated_partitions": under_replicated,
        "offline_partitions": offline,
        "partitions": rows.into_iter().take(limit).collect::<Vec<_>>(),
    });
    put_note(&mut output, truncation_note(limit.min(total), total, "partitions"));
    output
}

/// Settings most people ask about, surfaced first with a readable form.
const KEY_TOPIC_SETTINGS: &[&str] = &[
    "cleanup.policy",
    "retention.ms",
    "retention.bytes",
    "min.insync.replicas",
    "max.message.bytes",
    "segment.bytes",
    "compression.type",
];

fn readable_config(name: &str, value: &str) -> Option<String> {
    let number = value.parse::<i64>().ok()?;
    if number < 0 && (name.ends_with(".ms") || name.ends_with(".bytes")) {
        return Some("unlimited".to_string());
    }
    if name.ends_with(".ms") {
        return Some(human_duration(number as f64 / 1000.0));
    }
    if name.ends_with(".bytes") {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut size = number as f64;
        let mut unit = 0;
        while size >= 1024.0 && unit < UNITS.len() - 1 {
            size /= 1024.0;
            unit += 1;
        }
        return Some(format!("{} {}", round2(size), UNITS[unit]));
    }
    None
}

pub fn shape_topic_config(topic: &str, raw: &Value) -> Value {
    if raw.get("configSupported").and_then(Value::as_bool) == Some(false) {
        return json!({
            "topic": topic,
            "supported": false,
            "reason": raw.get("unsupportedReason").and_then(Value::as_str).unwrap_or("Topic configuration is unavailable."),
        });
    }
    let configs = raw.get("configs").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut overrides = BTreeMap::new();
    let mut defaults = BTreeMap::new();
    let mut key_settings = serde_json::Map::new();
    for (name, entry) in &configs {
        let sensitive = entry.get("isSensitive").and_then(Value::as_bool).unwrap_or(false);
        let value = if sensitive {
            Some("<redacted>".to_string())
        } else {
            entry.get("value").and_then(Value::as_str).map(str::to_string)
        };
        let is_default = entry.get("isDefault").and_then(Value::as_bool).unwrap_or(false)
            || entry.get("source").and_then(Value::as_str) == Some("DEFAULT_CONFIG");
        if KEY_TOPIC_SETTINGS.contains(&name.as_str()) {
            let mut setting = json!({ "value": value, "default": is_default });
            if let Some(readable) = value.as_deref().and_then(|value| readable_config(name, value)) {
                setting["readable"] = json!(readable);
            }
            key_settings.insert(name.clone(), setting);
        }
        if is_default {
            defaults.insert(name.clone(), json!(value));
        } else {
            let source = entry.get("source").and_then(Value::as_str).unwrap_or("UNKNOWN");
            overrides.insert(name.clone(), json!({ "value": value, "source": source }));
        }
    }
    json!({
        "topic": topic,
        "config_count": configs.len(),
        "key_settings": key_settings,
        "overrides": overrides,
        "defaults": defaults,
    })
}

// ---- consumer groups ----

fn members_value(group: &KafkaConsumerGroupSummary) -> Value {
    json!(group.member_count)
}

pub fn shape_consumer_groups(snapshot: &KafkaConsumerGroupSnapshot, limit: usize) -> Value {
    let mut groups = snapshot.groups.iter().collect::<Vec<_>>();
    groups.sort_by(|a, b| a.group_id.cmp(&b.group_id));
    let mut states = BTreeMap::<String, usize>::new();
    for group in &groups {
        *states.entry(group.state.clone()).or_default() += 1;
    }
    let rows = groups
        .iter()
        .take(limit)
        .map(|group| {
            let mut row = json!({
                "group": group.group_id,
                "state": group.state,
                "members": members_value(group),
                "topics": group.topics,
                "total_lag": group.total_lag,
            });
            if let Some(error) = group.error.as_deref() {
                row["error"] = json!(error);
            }
            row
        })
        .collect::<Vec<_>>();
    let unknown_lag = groups.iter().filter(|group| group.total_lag.is_none()).count();
    let mut output = json!({
        "group_count": groups.len(),
        "states": states,
        "total_lag": groups.iter().filter_map(|group| group.total_lag).sum::<i64>(),
        "groups": rows,
    });
    if unknown_lag > 0 {
        output["lag_note"] = json!(format!(
            "{unknown_lag} group(s) have unknown lag (no committed offsets or end offsets unavailable)."
        ));
    }
    put_note(&mut output, truncation_note(limit.min(groups.len()), groups.len(), "groups"));
    output
}

pub fn find_group<'a>(
    snapshot: &'a KafkaConsumerGroupSnapshot,
    group: &str,
) -> Result<&'a KafkaConsumerGroupSummary, String> {
    snapshot.groups.iter().find(|candidate| candidate.group_id == group).ok_or_else(|| {
        let mut names = snapshot.groups.iter().map(|group| group.group_id.as_str()).collect::<Vec<_>>();
        names.sort_unstable();
        let shown = names.iter().take(20).copied().collect::<Vec<_>>().join(", ");
        let more = if names.len() > 20 { format!(" (+{} more)", names.len() - 20) } else { String::new() };
        format!("Consumer group \"{group}\" was not found. Known groups: {shown}{more}")
    })
}

pub fn shape_consumer_group(group: &KafkaConsumerGroupSummary, limit: usize) -> Value {
    let mut partitions = group.partitions.iter().collect::<Vec<_>>();
    partitions.sort_by(|a, b| a.topic.cmp(&b.topic).then(a.partition.cmp(&b.partition)));
    let rows = partitions
        .iter()
        .take(limit)
        .map(|partition| {
            json!({
                "topic": partition.topic,
                "partition": partition.partition,
                "committed_offset": partition.current_offset,
                "end_offset": partition.end_offset,
                "lag": partition.lag,
            })
        })
        .collect::<Vec<_>>();
    let topics = aggregate_group_lag(group);
    let mut output = json!({
        "group": group.group_id,
        "state": group.state,
        "members": members_value(group),
        "total_lag": group.total_lag,
        "topics": topics.iter().map(LagRow::to_topic_json).collect::<Vec<_>>(),
        "partitions": rows,
    });
    if let Some(error) = group.error.as_deref() {
        output["error"] = json!(error);
    }
    put_note(&mut output, truncation_note(limit.min(partitions.len()), partitions.len(), "partitions"));
    output
}

// ---- lag aggregation ----

/// Lag of one consumer group on one topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LagRow {
    pub group: String,
    pub state: String,
    pub topic: String,
    pub partitions: usize,
    pub partitions_lagging: usize,
    /// `None` when any partition's lag is unknown.
    pub total_lag: Option<i64>,
    pub max_partition_lag: Option<i64>,
    pub max_lag_partition: Option<i32>,
}

impl LagRow {
    /// `spread` (slow consumer) versus `concentrated` (one partition holds
    /// nearly all lag, which points at a stuck partition or consumer).
    pub fn pattern(&self) -> Option<&'static str> {
        let (total, max) = (self.total_lag?, self.max_partition_lag?);
        if total <= 0 {
            return None;
        }
        if self.partitions <= 1 {
            return Some("single-partition");
        }
        if self.partitions_lagging == 1 || max * 10 >= total * 8 {
            Some("concentrated")
        } else {
            Some("spread")
        }
    }

    fn to_topic_json(&self) -> Value {
        json!({
            "topic": self.topic,
            "partitions": self.partitions,
            "partitions_lagging": self.partitions_lagging,
            "total_lag": self.total_lag,
            "max_partition_lag": self.max_partition_lag,
            "max_lag_partition": self.max_lag_partition,
            "pattern": self.pattern(),
        })
    }

    fn to_json(&self) -> Value {
        let mut row = self.to_topic_json();
        let object = row.as_object_mut().expect("lag row is an object");
        let mut ordered = serde_json::Map::new();
        ordered.insert("group".into(), json!(self.group));
        ordered.insert("state".into(), json!(self.state));
        ordered.append(object);
        Value::Object(ordered)
    }
}

fn aggregate_group_lag(group: &KafkaConsumerGroupSummary) -> Vec<LagRow> {
    let mut by_topic = BTreeMap::<&str, LagRow>::new();
    for partition in &group.partitions {
        let row = by_topic.entry(partition.topic.as_str()).or_insert_with(|| LagRow {
            group: group.group_id.clone(),
            state: group.state.clone(),
            topic: partition.topic.clone(),
            partitions: 0,
            partitions_lagging: 0,
            total_lag: Some(0),
            max_partition_lag: Some(0),
            max_lag_partition: None,
        });
        row.partitions += 1;
        match partition.lag {
            Some(lag) => {
                if lag > 0 {
                    row.partitions_lagging += 1;
                }
                row.total_lag = row.total_lag.map(|total| total + lag);
                if row.max_partition_lag.is_some_and(|max| lag > max) {
                    row.max_partition_lag = Some(lag);
                    row.max_lag_partition = Some(partition.partition);
                }
            }
            None => {
                row.total_lag = None;
            }
        }
    }
    by_topic.into_values().collect()
}

/// Lag per (group, topic), worst first: known lag descending, then the largest
/// single-partition lag, then name. Rows with unknown lag go last.
pub fn aggregate_lag(snapshot: &KafkaConsumerGroupSnapshot, group: Option<&str>) -> Vec<LagRow> {
    let mut rows = snapshot
        .groups
        .iter()
        .filter(|candidate| group.is_none_or(|group| candidate.group_id == group))
        .flat_map(aggregate_group_lag)
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.total_lag
            .is_some()
            .cmp(&a.total_lag.is_some())
            .then(b.total_lag.cmp(&a.total_lag))
            .then(b.max_partition_lag.cmp(&a.max_partition_lag))
            .then_with(|| a.group.cmp(&b.group))
            .then_with(|| a.topic.cmp(&b.topic))
    });
    rows
}

pub fn shape_lag(snapshot: &KafkaConsumerGroupSnapshot, group: Option<&str>, limit: usize) -> Value {
    let rows = aggregate_lag(snapshot, group);
    let groups = rows.iter().map(|row| row.group.as_str()).collect::<BTreeSet<_>>();
    let in_scope =
        || snapshot.groups.iter().filter(move |candidate| group.is_none_or(|group| candidate.group_id == group));
    // A group whose offsets failed to load is not idle; report it separately.
    let idle_groups = in_scope()
        .filter(|candidate| candidate.partitions.is_empty() && candidate.error.is_none())
        .map(|candidate| candidate.group_id.as_str())
        .collect::<Vec<_>>();
    let failed_groups = in_scope().filter(|candidate| candidate.error.is_some()).collect::<Vec<_>>();
    let mut output = json!({
        "total_lag": rows.iter().filter_map(|row| row.total_lag).sum::<i64>(),
        "groups": groups.len(),
        "rows_with_lag": rows.iter().filter(|row| row.total_lag.is_some_and(|lag| lag > 0)).count(),
        "rows": rows.iter().take(limit).map(LagRow::to_json).collect::<Vec<_>>(),
        "hint": "max_partition_lag separates a slow consumer (lag spread evenly across partitions, pattern=spread) \
                 from a stuck one (lag piled on one partition, pattern=concentrated).",
    });
    if !idle_groups.is_empty() {
        output["groups_without_offsets"] = json!(idle_groups);
    }
    if !failed_groups.is_empty() {
        output["groups_with_errors"] = json!(failed_groups.len());
        output["group_errors"] = json!(failed_groups
            .iter()
            .take(MAX_GROUP_ERROR_ROWS)
            .map(|failed| json!({ "group": failed.group_id, "error": failed.error }))
            .collect::<Vec<_>>());
    }
    put_note(&mut output, truncation_note(limit.min(rows.len()), rows.len(), "group/topic rows"));
    output
}

// ---- throughput ----

/// One consumer group snapshot taken at `at_ms` on a monotonic clock.
#[derive(Debug, Clone)]
pub struct ThroughputSample {
    pub at_ms: u64,
    pub snapshot: KafkaConsumerGroupSnapshot,
}

/// Monotonic clock with an injectable sleep so tests never wait.
pub trait SampleClock: Send + Sync {
    fn now_ms(&self) -> u64;
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Real clock backed by `tokio::time`.
pub struct TokioClock {
    origin: tokio::time::Instant,
}

impl Default for TokioClock {
    fn default() -> Self {
        Self { origin: tokio::time::Instant::now() }
    }
}

impl SampleClock for TokioClock {
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

pub fn clamp_sample_seconds(requested: Option<u64>) -> u64 {
    requested.unwrap_or(THROUGHPUT_DEFAULT_SECONDS).clamp(THROUGHPUT_MIN_SECONDS, THROUGHPUT_MAX_SECONDS)
}

/// Take two snapshots `interval` apart. Each sample is stamped when its fetch returns.
pub async fn sample_twice<F, Fut>(
    clock: &dyn SampleClock,
    interval: Duration,
    mut fetch: F,
) -> Result<(ThroughputSample, ThroughputSample), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<KafkaConsumerGroupSnapshot, String>>,
{
    let first = fetch().await?;
    let first = ThroughputSample { at_ms: clock.now_ms(), snapshot: first };
    clock.sleep(interval).await;
    let second = fetch().await?;
    let second = ThroughputSample { at_ms: clock.now_ms(), snapshot: second };
    Ok((first, second))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThroughputStatus {
    CaughtUp,
    Stalled,
    FallingBehind,
    CatchingUp,
    KeepingPace,
    Unknown,
}

impl ThroughputStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CaughtUp => "caught up",
            Self::Stalled => "stalled",
            Self::FallingBehind => "falling behind",
            Self::CatchingUp => "catching up",
            Self::KeepingPace => "keeping pace",
            Self::Unknown => "unknown",
        }
    }

    /// Lower is more urgent; used to sort worst first.
    fn severity(self) -> u8 {
        match self {
            Self::Stalled => 0,
            Self::FallingBehind => 1,
            Self::CatchingUp => 2,
            Self::KeepingPace => 3,
            Self::Unknown => 4,
            Self::CaughtUp => 5,
        }
    }
}

/// Status from the lag at the end of the window, messages consumed during it,
/// and the backlog change (end lag minus start lag).
pub fn classify_throughput(lag_end: i64, consumed: i64, backlog_delta: i64) -> ThroughputStatus {
    if lag_end == 0 {
        ThroughputStatus::CaughtUp
    } else if consumed == 0 {
        ThroughputStatus::Stalled
    } else if backlog_delta > 0 {
        ThroughputStatus::FallingBehind
    } else if backlog_delta < 0 {
        ThroughputStatus::CatchingUp
    } else {
        ThroughputStatus::KeepingPace
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThroughputRow {
    pub group: String,
    pub topic: String,
    pub produced_per_sec: Option<f64>,
    pub consumed_per_sec: Option<f64>,
    pub backlog_change_per_sec: Option<f64>,
    pub lag_start: Option<i64>,
    pub lag_end: Option<i64>,
    pub eta_seconds: Option<f64>,
    pub status: ThroughputStatus,
}

#[derive(Default)]
struct OffsetTotals {
    end: i64,
    committed: i64,
    lag: i64,
    complete: bool,
}

type PartitionKey<'a> = (&'a str, &'a str, i32);

fn partition_offsets(snapshot: &KafkaConsumerGroupSnapshot) -> BTreeMap<PartitionKey<'_>, (Option<i64>, Option<i64>)> {
    snapshot
        .groups
        .iter()
        .flat_map(|group| {
            group.partitions.iter().map(move |partition| {
                (
                    (group.group_id.as_str(), partition.topic.as_str(), partition.partition),
                    (partition.current_offset, partition.end_offset),
                )
            })
        })
        .collect()
}

/// Rates per (group, topic) between two samples. Only partitions present in
/// both samples with known offsets count; per-partition deltas are clamped at
/// zero so offset resets or topic recreation do not produce negative rates.
pub fn compute_throughput(
    first: &ThroughputSample,
    second: &ThroughputSample,
    group: Option<&str>,
) -> Vec<ThroughputRow> {
    let elapsed = second.at_ms.saturating_sub(first.at_ms) as f64 / 1000.0;
    let before = partition_offsets(&first.snapshot);
    let after = partition_offsets(&second.snapshot);
    let mut totals = BTreeMap::<(&str, &str), (OffsetTotals, OffsetTotals, i64, i64)>::new();
    for (key, (committed_after, end_after)) in &after {
        if group.is_some_and(|group| group != key.0) {
            continue;
        }
        let entry = totals.entry((key.0, key.1)).or_insert_with(|| {
            (
                OffsetTotals { complete: true, ..Default::default() },
                OffsetTotals { complete: true, ..Default::default() },
                0,
                0,
            )
        });
        let (start, end, produced, consumed) = entry;
        let previous = before.get(key);
        match (previous, committed_after, end_after) {
            (Some((Some(committed_before), Some(end_before))), Some(committed_after), Some(end_after)) => {
                start.lag += (end_before - committed_before).max(0);
                end.lag += (end_after - committed_after).max(0);
                start.end += end_before;
                end.end += end_after;
                start.committed += committed_before;
                end.committed += committed_after;
                *produced += (end_after - end_before).max(0);
                *consumed += (committed_after - committed_before).max(0);
            }
            _ => {
                start.complete = false;
                end.complete = false;
            }
        }
    }
    let mut rows = totals
        .into_iter()
        .map(|((group, topic), (start, end, produced, consumed))| {
            if !end.complete || elapsed <= 0.0 {
                return ThroughputRow {
                    group: group.to_string(),
                    topic: topic.to_string(),
                    produced_per_sec: None,
                    consumed_per_sec: None,
                    backlog_change_per_sec: None,
                    lag_start: start.complete.then_some(start.lag),
                    lag_end: end.complete.then_some(end.lag),
                    eta_seconds: None,
                    status: ThroughputStatus::Unknown,
                };
            }
            let backlog_delta = end.lag - start.lag;
            let backlog_rate = backlog_delta as f64 / elapsed;
            let eta_seconds = (end.lag > 0 && backlog_delta < 0).then(|| end.lag as f64 / -backlog_rate);
            ThroughputRow {
                group: group.to_string(),
                topic: topic.to_string(),
                produced_per_sec: Some(round2(produced as f64 / elapsed)),
                consumed_per_sec: Some(round2(consumed as f64 / elapsed)),
                backlog_change_per_sec: Some(round2(backlog_rate)),
                lag_start: Some(start.lag),
                lag_end: Some(end.lag),
                eta_seconds: eta_seconds.map(|eta| eta.round()),
                status: classify_throughput(end.lag, consumed, backlog_delta),
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        a.status
            .severity()
            .cmp(&b.status.severity())
            .then(b.lag_end.cmp(&a.lag_end))
            .then_with(|| a.group.cmp(&b.group))
            .then_with(|| a.topic.cmp(&b.topic))
    });
    rows
}

pub fn shape_throughput(
    first: &ThroughputSample,
    second: &ThroughputSample,
    group: Option<&str>,
    limit: usize,
) -> Value {
    let rows = compute_throughput(first, second, group);
    let mut statuses = BTreeMap::<&str, usize>::new();
    for row in &rows {
        *statuses.entry(row.status.as_str()).or_default() += 1;
    }
    let shown = rows
        .iter()
        .take(limit)
        .map(|row| {
            let mut value = json!({
                "group": row.group,
                "topic": row.topic,
                "status": row.status.as_str(),
                "produced_per_sec": row.produced_per_sec,
                "consumed_per_sec": row.consumed_per_sec,
                "backlog_change_per_sec": row.backlog_change_per_sec,
                "lag": row.lag_end,
                "lag_at_start": row.lag_start,
            });
            match (row.status, row.eta_seconds) {
                (_, Some(eta)) => {
                    value["time_to_catch_up"] = json!(human_duration(eta));
                    value["time_to_catch_up_seconds"] = json!(eta);
                }
                (ThroughputStatus::CaughtUp, None) => value["time_to_catch_up"] = json!("0s"),
                (ThroughputStatus::Stalled | ThroughputStatus::FallingBehind | ThroughputStatus::KeepingPace, None) => {
                    value["time_to_catch_up"] = json!("never at the current rate")
                }
                _ => {}
            }
            value
        })
        .collect::<Vec<_>>();
    let mut output = json!({
        "sample_seconds": round2(second.at_ms.saturating_sub(first.at_ms) as f64 / 1000.0),
        "statuses": statuses,
        "total_lag": rows.iter().filter_map(|row| row.lag_end).sum::<i64>(),
        "rows": shown,
        "hint": "Rates cover the partitions each group reads. Consumers commit every 5s by default, so short windows \
                 can report a healthy consumer as stalled; re-sample with a longer sample_seconds to confirm.",
    });
    put_note(&mut output, truncation_note(limit.min(rows.len()), rows.len(), "group/topic rows"));
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mq::types::KafkaConsumerGroupPartitionLag;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    fn partition(topic: &str, partition: i32, committed: i64, end: i64) -> KafkaConsumerGroupPartitionLag {
        KafkaConsumerGroupPartitionLag {
            topic: topic.into(),
            partition,
            current_offset: Some(committed),
            end_offset: Some(end),
            lag: Some((end - committed).max(0)),
        }
    }

    fn group(id: &str, partitions: Vec<KafkaConsumerGroupPartitionLag>) -> KafkaConsumerGroupSummary {
        let total_lag = partitions.iter().map(|partition| partition.lag).sum::<Option<i64>>();
        KafkaConsumerGroupSummary {
            group_id: id.into(),
            state: "STABLE".into(),
            member_count: Some(1),
            topics: partitions
                .iter()
                .map(|partition| partition.topic.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            lag_available: total_lag.is_some(),
            total_lag,
            partitions,
            ..Default::default()
        }
    }

    fn snapshot(groups: Vec<KafkaConsumerGroupSummary>) -> KafkaConsumerGroupSnapshot {
        KafkaConsumerGroupSnapshot { groups }
    }

    #[test]
    fn lag_is_worst_first_and_separates_stuck_from_slow() {
        let snap = snapshot(vec![
            group(
                "slow",
                vec![partition("orders", 0, 0, 300), partition("orders", 1, 0, 300), partition("orders", 2, 0, 300)],
            ),
            group(
                "stuck",
                vec![partition("orders", 0, 0, 1000), partition("orders", 1, 50, 50), partition("orders", 2, 9, 9)],
            ),
            group("fine", vec![partition("events", 0, 10, 10)]),
            group(
                "unknown",
                vec![KafkaConsumerGroupPartitionLag {
                    topic: "x".into(),
                    partition: 0,
                    current_offset: None,
                    end_offset: Some(5),
                    lag: None,
                }],
            ),
        ]);
        let rows = aggregate_lag(&snap, None);
        let order = rows.iter().map(|row| row.group.as_str()).collect::<Vec<_>>();
        assert_eq!(order, vec!["stuck", "slow", "fine", "unknown"]);
        assert_eq!(rows[0].total_lag, Some(1000));
        assert_eq!(rows[0].max_partition_lag, Some(1000));
        assert_eq!(rows[0].max_lag_partition, Some(0));
        assert_eq!(rows[0].pattern(), Some("concentrated"));
        assert_eq!(rows[1].total_lag, Some(900));
        assert_eq!(rows[1].max_partition_lag, Some(300));
        assert_eq!(rows[1].partitions_lagging, 3);
        assert_eq!(rows[1].pattern(), Some("spread"));
        assert_eq!(rows[2].pattern(), None);
        assert_eq!(rows[3].total_lag, None);

        let filtered = aggregate_lag(&snap, Some("slow"));
        assert_eq!(filtered.len(), 1);

        let shaped = shape_lag(&snap, None, 2);
        assert_eq!(shaped["total_lag"], 1900);
        assert_eq!(shaped["rows"].as_array().unwrap().len(), 2);
        assert_eq!(shaped["rows"][0]["group"], "stuck");
        assert_eq!(shaped["rows"][0]["max_partition_lag"], 1000);
        assert_eq!(shaped["truncated"], true);
        assert!(shaped["hint"].as_str().unwrap().contains("max_partition_lag"));
        assert!(shaped.get("groups_with_errors").is_none(), "no error fields when every group loaded");
    }

    #[test]
    fn lag_reports_groups_whose_offsets_failed_apart_from_idle_groups() {
        let failed = KafkaConsumerGroupSummary {
            error: Some("Committed offsets unavailable: TimeoutException".into()),
            ..group("broken", Vec::new())
        };
        let snap = snapshot(vec![group("idle", Vec::new()), failed, group("ok", vec![partition("t", 0, 1, 3)])]);

        let shaped = shape_lag(&snap, None, 10);
        assert_eq!(shaped["groups_without_offsets"], json!(["idle"]));
        assert_eq!(shaped["groups_with_errors"], 1);
        assert_eq!(shaped["group_errors"][0]["group"], "broken");
        assert!(shaped["group_errors"][0]["error"].as_str().unwrap().contains("Committed offsets unavailable"));
        assert_eq!(shaped["total_lag"], 2);

        let scoped = shape_lag(&snap, Some("ok"), 10);
        assert!(scoped.get("groups_with_errors").is_none(), "errors outside the requested group are not reported");
    }

    fn sample(at_ms: u64, groups: Vec<KafkaConsumerGroupSummary>) -> ThroughputSample {
        ThroughputSample { at_ms, snapshot: snapshot(groups) }
    }

    #[test]
    fn throughput_classifies_all_five_statuses() {
        // (group, committed0, end0, committed1, end1) over 10s on one partition.
        let cases = [
            ("caught-up", 100, 100, 150, 150, "caught up"),
            ("stalled", 100, 200, 100, 250, "stalled"),
            ("falling", 100, 200, 120, 300, "falling behind"),
            ("catching", 100, 200, 250, 260, "catching up"),
            ("pace", 100, 200, 150, 250, "keeping pace"),
        ];
        let first = sample(1_000, cases.iter().map(|c| group(c.0, vec![partition("t", 0, c.1, c.2)])).collect());
        let second = sample(11_000, cases.iter().map(|c| group(c.0, vec![partition("t", 0, c.3, c.4)])).collect());
        let rows = compute_throughput(&first, &second, None);
        for case in cases {
            let row = rows.iter().find(|row| row.group == case.0).unwrap();
            assert_eq!(row.status.as_str(), case.5, "{}", case.0);
        }
        let catching = rows.iter().find(|row| row.group == "catching").unwrap();
        assert_eq!(catching.produced_per_sec, Some(6.0));
        assert_eq!(catching.consumed_per_sec, Some(15.0));
        assert_eq!(catching.backlog_change_per_sec, Some(-9.0));
        assert_eq!(catching.lag_end, Some(10));
        // 10 messages left, shrinking 9/s.
        assert_eq!(catching.eta_seconds, Some(1.0));
        let falling = rows.iter().find(|row| row.group == "falling").unwrap();
        assert_eq!(falling.backlog_change_per_sec, Some(8.0));
        assert_eq!(falling.eta_seconds, None);
        // Worst first.
        let order = rows.iter().map(|row| row.status.as_str()).collect::<Vec<_>>();
        assert_eq!(order, vec!["stalled", "falling behind", "catching up", "keeping pace", "caught up"]);

        let shaped = shape_throughput(&first, &second, Some("stalled"), 10);
        assert_eq!(shaped["sample_seconds"], 10.0);
        assert_eq!(shaped["rows"].as_array().unwrap().len(), 1);
        assert_eq!(shaped["rows"][0]["time_to_catch_up"], "never at the current rate");
    }

    #[test]
    fn throughput_marks_partitions_missing_from_one_sample_unknown_and_clamps_resets() {
        let first = sample(0, vec![group("g", vec![partition("a", 0, 500, 600)])]);
        let second = sample(
            5_000,
            vec![group("g", vec![partition("a", 0, 10, 700)]), group("new", vec![partition("b", 0, 1, 2)])],
        );
        let rows = compute_throughput(&first, &second, None);
        let reset = rows.iter().find(|row| row.group == "g").unwrap();
        assert_eq!(reset.consumed_per_sec, Some(0.0), "offset reset must not produce a negative rate");
        assert_eq!(reset.produced_per_sec, Some(20.0));
        let new = rows.iter().find(|row| row.group == "new").unwrap();
        assert_eq!(new.status, ThroughputStatus::Unknown);
    }

    struct FakeClock {
        now: AtomicU64,
        sleeps: Mutex<Vec<Duration>>,
    }

    impl SampleClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }

        fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            self.sleeps.lock().unwrap().push(duration);
            self.now.fetch_add(duration.as_millis() as u64, Ordering::SeqCst);
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn sample_twice_uses_the_injected_clock_without_sleeping() {
        let clock = FakeClock { now: AtomicU64::new(42), sleeps: Mutex::new(Vec::new()) };
        let mut calls = 0;
        let started = std::time::Instant::now();
        let (first, second) = sample_twice(&clock, Duration::from_secs(30), || {
            calls += 1;
            let end = calls * 100;
            async move { Ok(snapshot(vec![group("g", vec![partition("t", 0, 0, end)])])) }
        })
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(*clock.sleeps.lock().unwrap(), vec![Duration::from_secs(30)]);
        assert_eq!((first.at_ms, second.at_ms), (42, 30_042));
        let rows = compute_throughput(&first, &second, None);
        assert_eq!(rows[0].produced_per_sec, Some(3.33));
        assert_eq!(rows[0].status, ThroughputStatus::Stalled);
    }

    #[test]
    fn sample_seconds_are_clamped() {
        assert_eq!(clamp_sample_seconds(None), 10);
        assert_eq!(clamp_sample_seconds(Some(0)), 2);
        assert_eq!(clamp_sample_seconds(Some(600)), 60);
        assert_eq!(clamp_sample_seconds(Some(15)), 15);
    }

    #[test]
    fn topics_are_sorted_capped_and_totalled() {
        let topics = (0..5)
            .map(|index| TopicInfo {
                name: format!("t{}", 4 - index),
                partitions: Some(3),
                replication_factor: Some(2),
                internal: index == 0,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let shaped = shape_topics(&topics, 3);
        assert_eq!(shaped["topic_count"], 5);
        assert_eq!(shaped["total_partitions"], 15);
        assert_eq!(shaped["internal_topic_count"], 1);
        let names =
            shaped["topics"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect::<Vec<_>>();
        assert_eq!(names, vec!["t0", "t1", "t2"]);
        assert_eq!(shaped["topics"][0]["replication_factor"], 2);
        assert_eq!(shaped["truncated"], true);
        assert!(shaped["note"].as_str().unwrap().contains("3 of 5"));
    }

    #[test]
    fn cluster_reports_controller_brokers_and_topic_count() {
        let cluster = ClusterInfo {
            cluster_id: Some("c1".into()),
            broker_count: 2,
            controller_id: Some(2),
            controller_host: Some("b2:9092".into()),
            brokers: vec![
                crate::mq::types::BrokerNode { id: 2, host: "b2".into(), port: 9092, ..Default::default() },
                crate::mq::types::BrokerNode { id: 1, host: "b1".into(), port: 9092, ..Default::default() },
            ],
            raw: Value::Null,
        };
        let topics = vec![TopicInfo { name: "a".into(), partitions: Some(3), ..Default::default() }];
        let shaped = shape_cluster(&cluster, Ok(&topics));
        assert_eq!(shaped["controller"]["id"], 2);
        assert_eq!(shaped["brokers"][0]["address"], "b1:9092");
        assert_eq!(shaped["topic_count"], 1);
        let failed = shape_cluster(&cluster, Err("timeout"));
        assert_eq!(failed["topic_count"], Value::Null);
        assert_eq!(failed["topic_count_error"], "timeout");
    }

    #[test]
    fn topic_partitions_flag_under_replicated_and_offline() {
        let stats = TopicStats {
            raw: json!({
                "name": "orders", "partitions": 2, "replicationFactor": 2, "totalMessages": 15,
                "partitionStats": [
                    {"partition": 1, "leader": -1, "replicas": [1, 2], "isr": [], "beginOffset": 0, "endOffset": 5, "messageCount": 5},
                    {"partition": 0, "leader": 1, "replicas": [1, 2], "isr": [1, 2], "beginOffset": 10, "endOffset": 20, "messageCount": 10}
                ]
            }),
            ..Default::default()
        };
        let shaped = shape_topic("orders", &stats, 10);
        assert_eq!(shaped["total_messages"], 15);
        assert_eq!(shaped["partitions"][0]["partition"], 0);
        assert_eq!(shaped["partitions"][0]["start_offset"], 10);
        assert_eq!(shaped["partitions"][1]["leader"], Value::Null);
        assert_eq!(shaped["offline_partitions"], json!([1]));
        assert_eq!(shaped["under_replicated_partitions"], json!([1]));
    }

    #[test]
    fn topic_config_highlights_key_settings_and_redacts_secrets() {
        let raw = json!({"configs": {
            "retention.ms": {"value": "604800000", "source": "DEFAULT_CONFIG", "isDefault": true, "isSensitive": false},
            "cleanup.policy": {"value": "compact", "source": "DYNAMIC_TOPIC_CONFIG", "isDefault": false, "isSensitive": false},
            "retention.bytes": {"value": "-1", "source": "DEFAULT_CONFIG", "isDefault": true, "isSensitive": false},
            "secret.thing": {"value": "s3cr3t", "source": "DYNAMIC_TOPIC_CONFIG", "isDefault": false, "isSensitive": true}
        }});
        let shaped = shape_topic_config("orders", &raw);
        assert_eq!(shaped["key_settings"]["retention.ms"]["readable"], "7d 0h");
        assert_eq!(shaped["key_settings"]["retention.bytes"]["readable"], "unlimited");
        assert_eq!(shaped["overrides"]["cleanup.policy"]["value"], "compact");
        assert_eq!(shaped["overrides"]["secret.thing"]["value"], "<redacted>");
        assert!(!shaped.to_string().contains("s3cr3t"));
        assert_eq!(shaped["defaults"]["retention.ms"], "604800000");
        let unsupported = shape_topic_config(
            "old",
            &json!({"configs": {}, "configSupported": false, "unsupportedReason": "old broker"}),
        );
        assert_eq!(unsupported["supported"], false);
    }

    #[test]
    fn consumer_group_views_total_and_find_groups() {
        let snap = snapshot(vec![
            group("b", vec![partition("t", 1, 0, 4), partition("t", 0, 0, 6)]),
            group("a", vec![partition("t", 0, 3, 3)]),
        ]);
        let listed = shape_consumer_groups(&snap, 10);
        assert_eq!(listed["group_count"], 2);
        assert_eq!(listed["total_lag"], 10);
        assert_eq!(listed["groups"][0]["group"], "a");
        assert_eq!(listed["states"]["STABLE"], 2);

        let detail = shape_consumer_group(find_group(&snap, "b").unwrap(), 10);
        assert_eq!(detail["partitions"][0]["partition"], 0);
        assert_eq!(detail["partitions"][0]["committed_offset"], 0);
        assert_eq!(detail["topics"][0]["max_partition_lag"], 6);
        assert!(find_group(&snap, "zzz").unwrap_err().contains("Known groups: a, b"));
    }

    #[test]
    fn non_kafka_connections_are_refused_with_a_pointer() {
        let mut connection: crate::models::connection::ConnectionConfig = serde_json::from_value(json!({
            "id": "r", "name": "Cache", "db_type": "redis", "host": "", "port": 0,
            "username": "", "password": "", "database": "0", "ssl": false
        }))
        .unwrap();
        let error = ensure_kafka_connection(&connection).unwrap_err();
        assert!(error.contains("dbx_list_connections") && error.contains("kafka"), "{error}");
        connection.db_type = crate::models::connection::DatabaseType::MessageQueue;
        connection.external_config =
            Some(json!({"systemKind": "rabbitmq", "adminUrl": "http://x", "auth": {"kind": "none"}}));
        assert!(ensure_kafka_connection(&connection).unwrap_err().contains("rabbitmq"));
        connection.external_config = Some(
            json!({"systemKind": "kafka", "adminUrl": "", "auth": {"kind": "none"}, "extra": {"bootstrapServers": "k:9092"}}),
        );
        assert!(ensure_kafka_connection(&connection).is_ok());
    }

    #[test]
    fn requests_round_trip_as_tagged_json() {
        let request = KafkaObserveRequest::TopicStats { topic: "orders".into() };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value, json!({"op": "topicStats", "topic": "orders"}));
        assert_eq!(
            serde_json::from_value::<KafkaObserveRequest>(json!({"op": "consumerGroups"})).unwrap(),
            KafkaObserveRequest::ConsumerGroups
        );
        let (path, body) = request.web_api_request("k1");
        assert_eq!(path, "/api/mq/topics/stats");
        assert_eq!(body["connectionId"], "k1");
        assert_eq!(body["topic"]["topic"], "orders");
        assert_eq!(KafkaObserveRequest::Topics.web_api_request("k1").1["ns"]["tenant"], "_kafka");
    }

    #[test]
    fn human_durations_are_compact() {
        assert_eq!(human_duration(42.4), "42s");
        assert_eq!(human_duration(125.0), "2m 5s");
        assert_eq!(human_duration(3725.0), "1h 2m");
    }
}
