//! Agent process owner shared by the agent-backed MQ adapters (Kafka, RocketMQ,
//! RabbitMQ).
//!
//! The registry caches one adapter per connection for as long as the
//! connection lives, so the adapter must survive its agent dying. An RPC
//! timeout kills the agent (the stdio stream cannot skip the late response),
//! and a crashed or exited agent leaves broken pipes behind. Either way the
//! client is discarded, killed and reaped off the async runtime, and the next
//! call starts a fresh agent with the same handshake and connect parameters.
//! Read-only RPCs that hit a broken transport are retried once on the fresh
//! agent; timeouts are not retried because that would double the wait.

use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use crate::db::agent_driver::{AgentDriverClient, AgentLaunchSpec};

/// How to bring a freshly spawned agent to the connected state.
#[derive(Debug, Clone)]
pub(crate) struct MqAgentConnectPlan {
    /// `params` of the `connect` RPC.
    pub connect_params: serde_json::Value,
    /// Timeout for `handshake` and `connect`.
    pub timeout: Option<Duration>,
}

pub(crate) struct MqAgentClient {
    label: &'static str,
    launch: AgentLaunchSpec,
    plan: MqAgentConnectPlan,
    client: Mutex<Option<AgentDriverClient>>,
}

/// Why an RPC left the agent unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentFailure {
    /// Timed out or canceled: the driver already killed the process.
    Timeout,
    /// Pipes closed, process gone or protocol stream lost.
    Transport,
}

/// Local failures of the stdio transport. An `Agent RPC error (...)` is an
/// answer from a healthy agent and never matches.
const TRANSPORT_FAILURE_MARKERS: &[&str] = &[
    "agent stdin not available",
    "agent stdout not available",
    "failed to write to agent stdin",
    "failed to write newline to agent stdin",
    "failed to flush agent stdin",
    "failed to read response from agent",
    "agent rpc task failed",
    "agent response missing both",
    "agent runtime terminated",
    "agent runtime is unavailable",
    "agent runtime unavailable",
];

fn classify_agent_failure(error: &str) -> Option<AgentFailure> {
    let lower = error.trim_start().to_ascii_lowercase();
    if lower.starts_with("agent rpc error (") {
        return None;
    }
    if lower.starts_with("agent rpc call timed out") || error == "Query canceled" {
        return Some(AgentFailure::Timeout);
    }
    TRANSPORT_FAILURE_MARKERS.iter().any(|marker| lower.contains(marker)).then_some(AgentFailure::Transport)
}

/// Agent RPCs that only read cluster state, so replaying one on a fresh agent is safe.
pub(crate) fn is_idempotent_read(method: &str) -> bool {
    const READ_PREFIXES: &[&str] = &["mq_list_", "mq_get_", "mq_describe_", "mq_query_", "mq_view_"];
    matches!(method, "test_connection" | "mq_overview") || READ_PREFIXES.iter().any(|prefix| method.starts_with(prefix))
}

/// Kill and reap a discarded agent without blocking the async runtime
/// (`AgentDriverClient::drop` kills the process and waits for it).
fn discard_agent(client: AgentDriverClient) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(move || drop(client));
        }
        Err(_) => drop(client),
    }
}

impl MqAgentClient {
    /// Spawn the agent (or take a prewarmed one), handshake and connect.
    pub async fn connect(
        label: &'static str,
        launch: AgentLaunchSpec,
        plan: MqAgentConnectPlan,
    ) -> Result<Self, String> {
        let client = start_agent(&launch, &plan).await?;
        Ok(Self::from_client(label, launch, plan, client))
    }

    /// Wrap an already connected client; `launch` and `plan` are used to replace it.
    pub fn from_client(
        label: &'static str,
        launch: AgentLaunchSpec,
        plan: MqAgentConnectPlan,
        client: AgentDriverClient,
    ) -> Self {
        Self { label, launch, plan, client: Mutex::new(Some(client)) }
    }

    /// Send one JSON-RPC call. A dead agent is replaced before the call; an agent
    /// broken by the call is discarded so the next call starts a fresh one.
    pub async fn call<T: DeserializeOwned + Send + 'static>(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Option<Duration>,
    ) -> Result<T, String> {
        let mut slot = self.client.lock().await;
        let mut retried = false;
        loop {
            if !slot.as_mut().is_some_and(AgentDriverClient::is_usable) {
                if let Some(dead) = slot.take() {
                    log::info!("[{}] agent is no longer usable; starting a fresh agent", self.label);
                    discard_agent(dead);
                }
                *slot = Some(start_agent(&self.launch, &self.plan).await?);
            }
            let client = slot.as_mut().expect("agent client was just ensured");
            let error = match client.call_with_timeout::<T>(method, params.clone(), timeout).await {
                Ok(result) => return Ok(result),
                Err(error) => error,
            };
            let failure =
                classify_agent_failure(&error).or_else(|| (!client.is_usable()).then_some(AgentFailure::Transport));
            let Some(failure) = failure else {
                return Err(error);
            };
            log::warn!("[{}] agent RPC {method} failed ({failure:?}); discarding the agent: {error}", self.label);
            if let Some(broken) = slot.take() {
                discard_agent(broken);
            }
            if failure == AgentFailure::Transport && !retried && is_idempotent_read(method) {
                retried = true;
                continue;
            }
            return Err(error);
        }
    }
}

async fn start_agent(launch: &AgentLaunchSpec, plan: &MqAgentConnectPlan) -> Result<AgentDriverClient, String> {
    let mut client = crate::agent_prewarm::spawn_agent_client(launch.clone()).await?;
    let _: serde_json::Value = client.call_with_timeout("handshake", serde_json::json!({}), plan.timeout).await?;
    let _: serde_json::Value = client.call_with_timeout("connect", plan.connect_params.clone(), plan.timeout).await?;
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python_agent(script: &str) -> (AgentLaunchSpec, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("dbx-mq-agent-{}.py", uuid::Uuid::new_v4()));
        std::fs::write(&path, script).expect("write fake agent");
        let python = if cfg!(windows) { "python" } else { "python3" };
        (AgentLaunchSpec::new(python).with_args([path.to_string_lossy().to_string()]), path)
    }

    fn plan() -> MqAgentConnectPlan {
        MqAgentConnectPlan {
            connect_params: serde_json::json!({ "connection": {} }),
            timeout: Some(Duration::from_secs(10)),
        }
    }

    /// Answers every RPC with its own pid. `mq_get_slow` hangs past the test
    /// timeout on the first agent only (the marker file is created by it), and
    /// `mq_get_crash` makes the first agent exit without answering.
    fn fake_agent(marker: &std::path::Path) -> String {
        format!(
            r#"import json, os, sys, time
marker = {marker:?}
first = not os.path.exists(marker)
open(marker, "a").close()
print(json.dumps({{"ready": True}}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    if first and method == "mq_get_slow":
        time.sleep(30)
    if first and method in ("mq_get_crash", "mq_create_crash"):
        sys.exit(3)
    if method == "mq_fail":
        reply = {{"error": {{"code": -1, "message": "broker said no"}}}}
    else:
        reply = {{"result": {{"pid": os.getpid(), "method": method}}}}
    reply.update({{"jsonrpc": "2.0", "id": request["id"]}})
    print(json.dumps(reply), flush=True)
"#,
            marker = marker.to_string_lossy()
        )
    }

    async fn client_for(marker: &std::path::Path) -> (MqAgentClient, std::path::PathBuf) {
        let (launch, script) = python_agent(&fake_agent(marker));
        let client = MqAgentClient::connect("test-mq", launch, plan()).await.expect("start fake agent");
        (client, script)
    }

    fn pid(value: &serde_json::Value) -> u64 {
        value["pid"].as_u64().expect("pid")
    }

    #[tokio::test]
    async fn timed_out_agent_is_replaced_on_the_next_call() {
        let marker = std::env::temp_dir().join(format!("dbx-mq-agent-marker-{}", uuid::Uuid::new_v4()));
        let (client, script) = client_for(&marker).await;
        let before: serde_json::Value = client.call("mq_describe_cluster", serde_json::json!({}), None).await.unwrap();

        let timed_out = client
            .call::<serde_json::Value>("mq_get_slow", serde_json::json!({}), Some(Duration::from_millis(300)))
            .await
            .expect_err("the hanging agent must time out");
        // The next call must not see "Agent stdin not available": it runs on a fresh agent.
        let after: serde_json::Value =
            client.call("mq_list_consumer_groups", serde_json::json!({}), None).await.unwrap();
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&marker);

        assert!(timed_out.starts_with("Agent RPC call timed out"), "{timed_out}");
        assert_eq!(after["method"], "mq_list_consumer_groups");
        assert_ne!(pid(&before), pid(&after), "a fresh agent process serves the call after a timeout");
        assert!(!process_alive(pid(&before)), "the timed-out agent is killed");
    }

    #[tokio::test]
    async fn crashed_agent_read_is_retried_once_on_a_fresh_agent() {
        let marker = std::env::temp_dir().join(format!("dbx-mq-agent-marker-{}", uuid::Uuid::new_v4()));
        let (client, script) = client_for(&marker).await;
        let before: serde_json::Value = client.call("mq_describe_cluster", serde_json::json!({}), None).await.unwrap();

        let retried: serde_json::Value = client.call("mq_get_crash", serde_json::json!({}), None).await.unwrap();
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&marker);

        assert_eq!(retried["method"], "mq_get_crash");
        assert_ne!(pid(&before), pid(&retried));
    }

    #[tokio::test]
    async fn crashed_agent_write_is_not_replayed_but_the_next_call_recovers() {
        let marker = std::env::temp_dir().join(format!("dbx-mq-agent-marker-{}", uuid::Uuid::new_v4()));
        let (client, script) = client_for(&marker).await;

        let failed = client.call::<serde_json::Value>("mq_create_crash", serde_json::json!({}), None).await;
        let next: serde_json::Value = client.call("mq_create_crash", serde_json::json!({}), None).await.unwrap();
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&marker);

        assert!(failed.is_err(), "a mutating RPC must not be replayed automatically");
        assert_eq!(next["method"], "mq_create_crash");
    }

    #[tokio::test]
    async fn agent_error_answers_keep_the_agent() {
        let marker = std::env::temp_dir().join(format!("dbx-mq-agent-marker-{}", uuid::Uuid::new_v4()));
        let (client, script) = client_for(&marker).await;
        let before: serde_json::Value = client.call("mq_describe_cluster", serde_json::json!({}), None).await.unwrap();

        let error = client.call::<serde_json::Value>("mq_fail", serde_json::json!({}), None).await.unwrap_err();
        let after: serde_json::Value = client.call("mq_describe_cluster", serde_json::json!({}), None).await.unwrap();
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&marker);

        assert!(error.contains("broker said no"), "{error}");
        assert_eq!(pid(&before), pid(&after), "an error answer does not restart the agent");
    }

    #[test]
    fn failures_are_classified_from_driver_errors() {
        assert_eq!(classify_agent_failure("Agent RPC call timed out at execute"), Some(AgentFailure::Timeout));
        assert_eq!(classify_agent_failure("Query canceled"), Some(AgentFailure::Timeout));
        assert_eq!(classify_agent_failure("Agent stdin not available"), Some(AgentFailure::Transport));
        assert_eq!(
            classify_agent_failure("Failed to write to agent stdin: Broken pipe (os error 32)"),
            Some(AgentFailure::Transport)
        );
        assert_eq!(
            classify_agent_failure("Failed to read response from agent: end of stream. agent process exited with 3"),
            Some(AgentFailure::Transport)
        );
        assert_eq!(classify_agent_failure("Agent RPC error (-1): stdin not available on broker"), None);
        assert_eq!(classify_agent_failure("Failed to deserialize agent result: missing field"), None);
    }

    #[test]
    fn only_read_rpcs_are_replayed() {
        for method in [
            "mq_get_consumer_group_snapshot",
            "mq_list_topics",
            "mq_describe_cluster",
            "mq_query_message_by_key",
            "mq_view_message",
            "mq_overview",
            "test_connection",
        ] {
            assert!(is_idempotent_read(method), "{method}");
        }
        for method in [
            "mq_create_topic",
            "mq_delete_topic",
            "mq_send_message",
            "mq_reset_consumer_group_offsets",
            "mq_peek_messages",
        ] {
            assert!(!is_idempotent_read(method), "{method}");
        }
    }

    fn process_alive(pid: u64) -> bool {
        if cfg!(windows) {
            return false;
        }
        // Reaping happens on a blocking task; give it a moment.
        for _ in 0..50 {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }
}
