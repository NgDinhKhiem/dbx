//! Pre-spawned ("spare") agent processes.
//!
//! Starting a JVM agent is the fixed cost of every agent-backed connection:
//! process start, JVM boot and the first class loading run before the agent
//! can send `{"ready":true}`. On a cold disk cache (first use after boot or a
//! fresh driver install) that can take seconds.
//!
//! A spare is only a started, idle agent process. Warming one never opens a
//! connection to a user database or broker: the connect RPC is sent by the
//! adapter after it takes the spare. Memory stays bounded:
//! - at most one spare per launch spec and [`MAX_SPARES`] in total;
//! - an unused spare is killed after [`SPARE_IDLE_TTL`];
//! - taking a spare removes it, so a live connection never also holds a spare.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::db::agent_driver::{AgentDriverClient, AgentLaunchSpec};

/// Upper bound on idle spare processes kept at once (one per agent kind).
pub const MAX_SPARES: usize = 2;
/// An unused spare is killed after this long so warming never pins memory.
pub const SPARE_IDLE_TTL: Duration = Duration::from_secs(10 * 60);

type SpareResult = Option<Result<AgentDriverClient, String>>;

struct SpareEntry {
    /// Held by the warming task until the spawn finishes, so a taker waits for
    /// an in-flight spawn instead of starting a second JVM.
    client: Arc<tokio::sync::Mutex<SpareResult>>,
    created_at: Instant,
    /// Set by [`AgentSparePool::clear`] so an in-flight spawn is killed as soon as it finishes.
    cleared: AtomicBool,
}

/// Outcome of a prewarm request, mainly for logs and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrewarmOutcome {
    /// A new spare process is being started.
    Started,
    /// A spare for this launch spec already exists or is starting.
    AlreadyWarm,
    /// The spare limit is reached; nothing was started.
    AtCapacity,
}

/// Decide what a prewarm request should do. Pure so the policy is testable.
pub fn prewarm_decision(has_entry_for_spec: bool, live_entries: usize, max_spares: usize) -> PrewarmOutcome {
    if has_entry_for_spec {
        PrewarmOutcome::AlreadyWarm
    } else if live_entries >= max_spares {
        PrewarmOutcome::AtCapacity
    } else {
        PrewarmOutcome::Started
    }
}

/// Whether a spare created at `created_at` is still worth handing out.
pub fn spare_is_fresh(created_at: Instant, now: Instant, ttl: Duration) -> bool {
    now.saturating_duration_since(created_at) < ttl
}

pub struct AgentSparePool {
    entries: Mutex<HashMap<AgentLaunchSpec, Arc<SpareEntry>>>,
    max_spares: usize,
    ttl: Duration,
}

impl AgentSparePool {
    pub fn new(max_spares: usize, ttl: Duration) -> Self {
        Self { entries: Mutex::new(HashMap::new()), max_spares, ttl }
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<AgentLaunchSpec, Arc<SpareEntry>>> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Number of spare slots (ready or starting).
    pub fn len(&self) -> usize {
        self.lock_entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Start a spare agent process for `launch` in the background unless one
    /// already exists. Returns immediately.
    pub fn prewarm(self: &Arc<Self>, launch: AgentLaunchSpec) -> PrewarmOutcome {
        let entry = {
            let mut entries = self.lock_entries();
            let now = Instant::now();
            entries.retain(|_, entry| spare_is_fresh(entry.created_at, now, self.ttl));
            let outcome = prewarm_decision(entries.contains_key(&launch), entries.len(), self.max_spares);
            if outcome != PrewarmOutcome::Started {
                return outcome;
            }
            let entry = Arc::new(SpareEntry {
                client: Arc::new(tokio::sync::Mutex::new(None)),
                created_at: Instant::now(),
                cleared: AtomicBool::new(false),
            });
            entries.insert(launch.clone(), entry.clone());
            entry
        };
        // Take the slot lock before the task runs so a concurrent `take` waits for this spawn.
        let guard =
            entry.client.clone().try_lock_owned().expect("a freshly created spare slot is never locked by anyone else");
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            let started = Instant::now();
            let mut guard = guard;
            let result = AgentDriverClient::spawn(launch.clone()).await;
            match &result {
                Ok(_) => log::info!(
                    "Prewarmed agent process {} in {} ms",
                    launch.program.display(),
                    started.elapsed().as_millis()
                ),
                Err(error) => log::warn!("Agent prewarm failed for {}: {error}", launch.program.display()),
            }
            if entry.cleared.load(Ordering::Acquire) {
                if let Ok(mut client) = result {
                    client.kill();
                }
                return;
            }
            let failed = result.is_err();
            *guard = Some(result);
            drop(guard);
            if failed {
                pool.remove_if_current(&launch, &entry);
                return;
            }
            // Retire the spare if nobody took it within the TTL.
            tokio::time::sleep(pool.ttl).await;
            if pool.remove_if_current(&launch, &entry) {
                if let Some(Ok(mut client)) = entry.client.lock().await.take() {
                    client.kill();
                }
                log::debug!("Retired unused prewarmed agent process {}", launch.program.display());
            }
        });
        PrewarmOutcome::Started
    }

    fn remove_if_current(&self, launch: &AgentLaunchSpec, entry: &Arc<SpareEntry>) -> bool {
        let mut entries = self.lock_entries();
        if entries.get(launch).is_some_and(|current| Arc::ptr_eq(current, entry)) {
            entries.remove(launch);
            true
        } else {
            false
        }
    }

    /// Take the spare process for `launch`, waiting for an in-flight prewarm.
    /// Returns `None` when there is no usable spare.
    pub async fn take(&self, launch: &AgentLaunchSpec) -> Option<AgentDriverClient> {
        let entry = self.lock_entries().remove(launch)?;
        if !spare_is_fresh(entry.created_at, Instant::now(), self.ttl) {
            return None;
        }
        let taken = entry.client.lock().await.take();
        match taken {
            Some(Ok(mut client)) => {
                if client.has_exited() {
                    log::warn!("Prewarmed agent process {} exited before use", launch.program.display());
                    None
                } else {
                    Some(client)
                }
            }
            _ => None,
        }
    }

    /// Use a spare process when one is warm, otherwise spawn a new one.
    pub async fn take_or_spawn(&self, launch: AgentLaunchSpec) -> Result<AgentDriverClient, String> {
        if let Some(client) = self.take(&launch).await {
            log::debug!("Using prewarmed agent process {}", launch.program.display());
            return Ok(client);
        }
        AgentDriverClient::spawn(launch).await
    }

    /// Kill every spare (driver uninstall/upgrade, shutdown).
    pub fn clear(&self) {
        let drained = std::mem::take(&mut *self.lock_entries());
        for (_, entry) in drained {
            // An in-flight spawn holds the slot lock and kills its process when it sees the flag.
            entry.cleared.store(true, Ordering::Release);
            if let Ok(mut slot) = entry.client.try_lock() {
                if let Some(Ok(mut client)) = slot.take() {
                    client.kill();
                }
            }
        }
    }
}

static SPARE_POOL: LazyLock<Arc<AgentSparePool>> =
    LazyLock::new(|| Arc::new(AgentSparePool::new(MAX_SPARES, SPARE_IDLE_TTL)));

/// Process-wide spare pool shared by the desktop and web servers.
pub fn spare_pool() -> &'static Arc<AgentSparePool> {
    &SPARE_POOL
}

/// Spawn an agent, reusing a prewarmed process for the same launch spec when available.
pub async fn spawn_agent_client(launch: AgentLaunchSpec) -> Result<AgentDriverClient, String> {
    spare_pool().take_or_spawn(launch).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python_agent(script: &str) -> (AgentLaunchSpec, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("dbx-prewarm-{}.py", uuid::Uuid::new_v4()));
        std::fs::write(&path, script).expect("write fake agent");
        let python = if cfg!(windows) { "python" } else { "python3" };
        (AgentLaunchSpec::new(python).with_args([path.to_string_lossy().to_string()]), path)
    }

    const READY_AGENT: &str = r#"import json, os, sys
print(json.dumps({"ready": True}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {"pid": os.getpid()}}), flush=True)
"#;

    #[test]
    fn decision_respects_existing_spare_and_capacity() {
        assert_eq!(prewarm_decision(true, 1, 2), PrewarmOutcome::AlreadyWarm);
        assert_eq!(prewarm_decision(false, 2, 2), PrewarmOutcome::AtCapacity);
        assert_eq!(prewarm_decision(false, 1, 2), PrewarmOutcome::Started);
        assert_eq!(prewarm_decision(false, 0, 0), PrewarmOutcome::AtCapacity);
    }

    #[test]
    fn spare_freshness_uses_ttl() {
        let created = Instant::now();
        let ttl = Duration::from_secs(60);
        assert!(spare_is_fresh(created, created + Duration::from_secs(59), ttl));
        assert!(!spare_is_fresh(created, created + ttl, ttl));
        // A clock that appears to go backwards never expires a spare.
        assert!(spare_is_fresh(created + Duration::from_secs(1), created, ttl));
    }

    #[tokio::test]
    async fn take_reuses_the_prewarmed_process_once() {
        let pool = Arc::new(AgentSparePool::new(2, Duration::from_secs(60)));
        let (launch, path) = python_agent(READY_AGENT);

        assert_eq!(pool.prewarm(launch.clone()), PrewarmOutcome::Started);
        assert_eq!(pool.prewarm(launch.clone()), PrewarmOutcome::AlreadyWarm);

        let mut warm = pool.take(&launch).await.expect("take waits for the in-flight spawn");
        assert!(pool.is_empty(), "taking a spare removes it");
        let warm_pid: serde_json::Value = warm.call("ping", serde_json::json!({})).await.expect("spare answers");

        // No spare left: the next caller gets a fresh process.
        assert!(pool.take(&launch).await.is_none());
        let mut fresh = pool.take_or_spawn(launch.clone()).await.expect("spawn fresh agent");
        let fresh_pid: serde_json::Value = fresh.call("ping", serde_json::json!({})).await.expect("fresh answers");
        assert_ne!(warm_pid, fresh_pid);

        drop(warm);
        drop(fresh);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn capacity_bounds_the_number_of_spares() {
        let pool = Arc::new(AgentSparePool::new(1, Duration::from_secs(60)));
        let (first, first_path) = python_agent(READY_AGENT);
        let (second, second_path) = python_agent(READY_AGENT);

        assert_eq!(pool.prewarm(first.clone()), PrewarmOutcome::Started);
        assert_eq!(pool.prewarm(second.clone()), PrewarmOutcome::AtCapacity);
        assert_eq!(pool.len(), 1);

        pool.clear();
        assert!(pool.is_empty());
        let _ = std::fs::remove_file(first_path);
        let _ = std::fs::remove_file(second_path);
    }

    #[tokio::test]
    async fn failed_prewarm_falls_back_to_a_regular_spawn() {
        let pool = Arc::new(AgentSparePool::new(2, Duration::from_secs(60)));
        let missing = AgentLaunchSpec::new("/nonexistent/dbx-agent-for-prewarm-test");

        assert_eq!(pool.prewarm(missing.clone()), PrewarmOutcome::Started);
        assert!(pool.take(&missing).await.is_none());
        assert!(pool.take_or_spawn(missing).await.is_err());
    }

    #[tokio::test]
    async fn exited_spare_is_not_handed_out() {
        let pool = Arc::new(AgentSparePool::new(2, Duration::from_secs(60)));
        let (launch, path) = python_agent(
            r#"import json
print(json.dumps({"ready": True}), flush=True)
"#,
        );
        assert_eq!(pool.prewarm(launch.clone()), PrewarmOutcome::Started);
        // Wait for the spawn to finish, then for the fake agent to exit.
        {
            let entry = pool.lock_entries().get(&launch).cloned().expect("spare slot");
            let _ready = entry.client.lock().await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(pool.take(&launch).await.is_none());
        let _ = std::fs::remove_file(path);
    }
}
