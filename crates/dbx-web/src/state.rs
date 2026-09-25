use dbx_core::connection::AppState;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{broadcast, watch, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::sse::TransferProgressChannel;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebExportFile {
    pub file_path: String,
    pub download_filename: String,
    pub format: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NacosImportContext {
    pub owner_session: Option<String>,
    pub connection_id: String,
    pub target_namespace: String,
    pub plan_hash: String,
}

pub struct WebState {
    pub app: Arc<AppState>,
    pub data_dir: PathBuf,
    pub public_base_path: String,
    pub password_disabled: bool,
    /// `DBX_DEMO_MODE`：公网演示部署的封锁开关（见 `demo` 模块）。
    pub demo_mode: bool,
    pub password_hash: RwLock<Option<String>>,
    /// `DBX_PASSWORD` is set: it replaces the stored hash on every start, so the
    /// password cannot be changed through the API.
    pub password_managed_by_env: bool,
    /// One-time token required by `/api/auth/setup` from non-local clients;
    /// generated at startup when no password is configured.
    pub setup_token: Option<String>,
    pub sessions: crate::session::SessionStore,
    pub sse_channels: RwLock<HashMap<String, broadcast::Sender<String>>>,
    pub transfer_progress_channels: RwLock<HashMap<String, Arc<TransferProgressChannel>>>,
    pub table_import_channels: RwLock<HashMap<String, watch::Sender<String>>>,
    pub sql_file_executions: RwLock<HashMap<String, CancellationToken>>,
    pub managed_sql_previews: crate::routes::sql_file::ManagedSqlPreviews,
    pub nacos_imports: RwLock<HashMap<String, NacosImportContext>>,
    pub login_rate_limit: crate::rate_limit::LoginRateLimiter,
    /// Caps concurrent Argon2 hashing/verification on the blocking pool.
    pub argon2_permits: Arc<Semaphore>,
    pub security: crate::security::SecurityConfig,
    /// Completed Web export temp files waiting for the browser download.
    pub export_files: RwLock<HashMap<String, WebExportFile>>,
    pub ssh_prompts: Arc<crate::ssh_prompt::SshPromptHub>,
    pub migration_ready: Arc<AtomicBool>,
}

/// Concurrent Argon2 operations: half the CPUs, between 1 and 4.
pub fn argon2_concurrency() -> usize {
    std::thread::available_parallelism().map(|count| count.get() / 2).unwrap_or(1).clamp(1, 4)
}

impl WebState {
    pub async fn remove_sse_channel(&self, id: &str) {
        self.sse_channels.write().await.remove(id);
    }

    /// Creates a login session; sessions evicted by the cap lose their
    /// session-scoped credentials.
    pub fn create_session(&self) -> String {
        let (token, evicted) = self.sessions.create();
        self.release_session_owners(&evicted);
        token
    }

    /// Returns the session's revocation token when it is valid, recording
    /// activity. Expired sessions are removed along with their credentials.
    pub fn validate_session(&self, token: &str) -> Option<CancellationToken> {
        match self.sessions.validate(token) {
            crate::session::SessionLookup::Valid(revoked) => Some(revoked),
            crate::session::SessionLookup::Expired => {
                self.release_session_owners(&[token.to_string()]);
                None
            }
            crate::session::SessionLookup::Missing => None,
        }
    }

    pub fn revoke_session(&self, token: &str) {
        self.sessions.remove(token);
        self.release_session_owners(&[token.to_string()]);
    }

    /// Revokes every session except `keep` (used after a password change).
    pub fn revoke_other_sessions(&self, keep: Option<&str>) -> usize {
        let removed = self.sessions.remove_all_except(keep);
        self.release_session_owners(&removed);
        removed.len()
    }

    /// Periodic cleanup of expired sessions and idle rate-limit entries.
    pub fn sweep_sessions(&self) {
        let expired = self.sessions.sweep();
        self.release_session_owners(&expired);
        self.login_rate_limit.sweep();
    }

    fn release_session_owners(&self, tokens: &[String]) {
        for token in tokens {
            // Session-scoped temporary passwords belong to the login session.
            self.app.session_credentials.clear_owner(token);
        }
    }

    /// Test helper: full field set so new WebState fields don't break scattered test fixtures.
    #[cfg(test)]
    pub fn for_tests(app: Arc<AppState>, data_dir: PathBuf) -> Self {
        Self {
            app,
            data_dir,
            public_base_path: "/".to_string(),
            password_disabled: false,
            demo_mode: false,
            password_hash: RwLock::new(None),
            password_managed_by_env: false,
            setup_token: None,
            sessions: crate::session::SessionStore::default(),
            sse_channels: RwLock::new(HashMap::new()),
            transfer_progress_channels: RwLock::new(HashMap::new()),
            table_import_channels: RwLock::new(HashMap::new()),
            sql_file_executions: RwLock::new(HashMap::new()),
            managed_sql_previews: Default::default(),
            nacos_imports: RwLock::new(HashMap::new()),
            login_rate_limit: crate::rate_limit::LoginRateLimiter::default(),
            argon2_permits: Arc::new(Semaphore::new(argon2_concurrency())),
            security: crate::security::SecurityConfig::default(),
            export_files: RwLock::new(HashMap::new()),
            ssh_prompts: Arc::new(crate::ssh_prompt::SshPromptHub::new()),
            migration_ready: Arc::new(AtomicBool::new(true)),
        }
    }
}
