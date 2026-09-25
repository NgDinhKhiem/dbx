//! Backend authorization for user-chosen paths outside the DBX data directory.
//!
//! Commands such as `read_external_sql_file`, `write_external_sql_file`,
//! `preview_sql_file` and `plugin_file_open` take a path from the webview. The
//! webview is not trusted to name arbitrary paths, so every such command checks
//! the path against grants that only the backend can create:
//!
//! * native file/folder dialogs opened by the backend (`pick_external_files`,
//!   `pick_external_directory`, `save_external_sql_file`),
//! * OS hand-offs: launch arguments, single-instance arguments, macOS
//!   open-file events and files dropped onto a window,
//! * an explicit native confirmation dialog (`request_external_path_access`,
//!   or the fallback prompt of user-initiated opens) for paths typed by the
//!   user or remembered by the frontend before grants existed.
//!
//! File and directory grants persist in `external-path-grants.json` inside the
//! data directory so restored editor tabs and SQL folders keep working after a
//! restart. Paths inside the DBX data directory itself are never granted: it
//! holds the app database, secrets and installed plugin code.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use dbx_core::connection::AppState;

const GRANTS_FILE_NAME: &str = "external-path-grants.json";
const GRANTS_FILE_VERSION: u32 = 1;
const MAX_PERSISTED_FILE_GRANTS: usize = 2000;
const MAX_PERSISTED_DIRECTORY_GRANTS: usize = 500;
const MAX_PROMPT_PATHS: usize = 50;
const MAX_PROMPT_LISTED_PATHS: usize = 8;
const LEGACY_MIGRATION_WAIT: Duration = Duration::from_secs(15);
pub const NOT_AUTHORIZED_ERROR_PREFIX: &str = "EXTERNAL_PATH_NOT_AUTHORIZED";

/// Text-like file types the external SQL editor may open or save. The SQL
/// folder browser supports custom filters (`*.sh`, `*.py`, ...), so this is
/// broader than `.sql`, but it excludes extension-less files (`.ssh/id_rsa`,
/// `.zshrc`, `authorized_keys`), key material, databases and binaries.
pub const EXTERNAL_TEXT_FILE_EXTENSIONS: &[&str] = &[
    "sql",
    "psql",
    "pgsql",
    "mysql",
    "plsql",
    "pls",
    "pks",
    "pkb",
    "tsql",
    "ddl",
    "dml",
    "hql",
    "cql",
    "prql",
    "ksql",
    "flux",
    "promql",
    "kql",
    "sparql",
    "cypher",
    "cyp",
    "gql",
    "graphql",
    "txt",
    "text",
    "md",
    "markdown",
    "log",
    "csv",
    "tsv",
    "json",
    "jsonl",
    "ndjson",
    "json5",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "xml",
    "properties",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    "py",
    "js",
    "mjs",
    "cjs",
    "ts",
    "rb",
    "go",
    "rs",
    "java",
    "kt",
    "scala",
    "lua",
    "r",
    "redis",
    "mongo",
];

pub fn has_external_text_file_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| EXTERNAL_TEXT_FILE_EXTENSIONS.iter().any(|allowed| extension.eq_ignore_ascii_case(allowed)))
        .unwrap_or(false)
}

pub fn ensure_external_text_file_extension(path: &Path) -> Result<(), String> {
    if has_external_text_file_extension(path) {
        Ok(())
    } else {
        Err(format!("{NOT_AUTHORIZED_ERROR_PREFIX}: unsupported file type for the external editor: {}", path.display()))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedGrants {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    directories: Vec<String>,
}

#[derive(Debug, Default)]
struct Grants {
    /// Most recent first; persisted.
    files: Vec<PathBuf>,
    /// Recursive directory grants, most recent first; persisted.
    directories: Vec<PathBuf>,
    /// Session-only grants (e.g. temporary SQL ZIP extraction output).
    session_files: HashSet<PathBuf>,
}

pub struct ExternalPathAccess {
    storage_path: Option<PathBuf>,
    forbidden_roots: Vec<PathBuf>,
    grants: Mutex<Grants>,
    legacy_migration_pending: bool,
    legacy_migration_done: AtomicBool,
    legacy_migration_notify: tokio::sync::Notify,
    /// Serializes native confirmation prompts so concurrent requests for the
    /// same path show one dialog.
    prompt_lock: tokio::sync::Mutex<()>,
}

/// Absolute, symlink-resolved form used for grants and checks. Missing files
/// resolve through their (existing) parent so a deleted file can be recreated.
pub fn normalize_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Some(canonical);
    }
    let file_name = path.file_name()?;
    let parent = std::fs::canonicalize(path.parent()?).ok()?;
    Some(parent.join(file_name))
}

impl ExternalPathAccess {
    pub fn load(data_dir: &Path) -> Self {
        let storage_path = data_dir.join(GRANTS_FILE_NAME);
        let (grants, legacy_migration_pending) = match std::fs::read(&storage_path) {
            Ok(bytes) => (Self::grants_from_bytes(&bytes), false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Grants::default(), true),
            Err(error) => {
                log::warn!("Failed to read external path grants: {error}");
                (Grants::default(), false)
            }
        };
        let mut forbidden_roots = vec![data_dir.to_path_buf()];
        if let Ok(canonical) = std::fs::canonicalize(data_dir) {
            forbidden_roots.push(canonical);
        }
        Self {
            storage_path: Some(storage_path),
            forbidden_roots,
            grants: Mutex::new(grants),
            legacy_migration_pending,
            legacy_migration_done: AtomicBool::new(!legacy_migration_pending),
            legacy_migration_notify: tokio::sync::Notify::new(),
            prompt_lock: tokio::sync::Mutex::new(()),
        }
    }

    #[cfg(test)]
    fn in_memory(forbidden_roots: Vec<PathBuf>) -> Self {
        Self {
            storage_path: None,
            forbidden_roots,
            grants: Mutex::new(Grants::default()),
            legacy_migration_pending: false,
            legacy_migration_done: AtomicBool::new(true),
            legacy_migration_notify: tokio::sync::Notify::new(),
            prompt_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn grants_from_bytes(bytes: &[u8]) -> Grants {
        let persisted: PersistedGrants = serde_json::from_slice(bytes).unwrap_or_default();
        Grants {
            files: persisted.files.into_iter().map(PathBuf::from).filter(|path| path.is_absolute()).collect(),
            directories: persisted
                .directories
                .into_iter()
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .collect(),
            session_files: HashSet::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Grants> {
        self.grants.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_forbidden(&self, normalized: &Path) -> bool {
        self.forbidden_roots.iter().any(|root| normalized.starts_with(root))
    }

    fn persist(&self, grants: &Grants) {
        let Some(path) = &self.storage_path else { return };
        let persisted = PersistedGrants {
            version: GRANTS_FILE_VERSION,
            files: grants.files.iter().map(|path| path.to_string_lossy().into_owned()).collect(),
            directories: grants.directories.iter().map(|path| path.to_string_lossy().into_owned()).collect(),
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&persisted) else { return };
        let temp = path.with_extension("json.tmp");
        let result = std::fs::write(&temp, bytes).and_then(|()| std::fs::rename(&temp, path));
        if let Err(error) = result {
            log::warn!("Failed to persist external path grants: {error}");
            let _ = std::fs::remove_file(&temp);
        }
    }

    /// Grants one file. Returns false for relative paths and paths inside the
    /// DBX data directory.
    pub fn grant_file(&self, path: &Path) -> bool {
        let Some(normalized) = normalize_path(path) else { return false };
        if self.is_forbidden(&normalized) || normalized.is_dir() {
            return false;
        }
        let mut grants = self.lock();
        grants.files.retain(|existing| existing != &normalized);
        grants.files.insert(0, normalized);
        grants.files.truncate(MAX_PERSISTED_FILE_GRANTS);
        self.persist(&grants);
        true
    }

    /// Grants a directory recursively.
    pub fn grant_directory(&self, path: &Path) -> bool {
        let Some(normalized) = normalize_path(path) else { return false };
        if !normalized.is_dir() || self.is_forbidden(&normalized) {
            return false;
        }
        // Granting a parent of the data directory would expose it through
        // `starts_with`; the forbidden-root check in `is_*_allowed` still
        // rejects every path inside it.
        let mut grants = self.lock();
        grants.directories.retain(|existing| existing != &normalized);
        grants.directories.insert(0, normalized);
        grants.directories.truncate(MAX_PERSISTED_DIRECTORY_GRANTS);
        self.persist(&grants);
        true
    }

    /// Grants a file for this process only (not persisted).
    pub fn grant_session_file(&self, path: &Path) {
        if let Some(normalized) = normalize_path(path) {
            if !self.is_forbidden(&normalized) {
                self.lock().session_files.insert(normalized);
            }
        }
    }

    /// Returns the normalized path when access to the file is granted.
    pub fn allowed_file(&self, path: &Path) -> Option<PathBuf> {
        let normalized = normalize_path(path)?;
        if self.is_forbidden(&normalized) {
            return None;
        }
        let grants = self.lock();
        let allowed = grants.files.iter().any(|file| file == &normalized)
            || grants.session_files.contains(&normalized)
            || grants.directories.iter().any(|directory| normalized.starts_with(directory));
        allowed.then_some(normalized)
    }

    /// Returns the normalized directory when it is (inside) a granted directory.
    pub fn allowed_directory(&self, path: &Path) -> Option<PathBuf> {
        let normalized = normalize_path(path)?;
        if self.is_forbidden(&normalized) {
            return None;
        }
        let grants = self.lock();
        grants.directories.iter().any(|directory| normalized.starts_with(directory)).then_some(normalized)
    }

    fn is_allowed(&self, path: &Path, directory: bool) -> bool {
        if directory {
            self.allowed_directory(path).is_some()
        } else {
            self.allowed_file(path).is_some()
        }
    }

    /// One-time import of paths the pre-authorization app already used, read
    /// from backend storage written by that version (restored editor tabs and
    /// global-search roots). Runs only when no grants file existed at launch.
    /// Access checks wait (bounded) for the one-time migration so restored
    /// tabs do not race it into a consent prompt.
    async fn wait_for_legacy_migration(&self) {
        let notified = self.legacy_migration_notify.notified();
        if self.legacy_migration_done.load(Ordering::Acquire) {
            return;
        }
        let _ = tokio::time::timeout(LEGACY_MIGRATION_WAIT, notified).await;
    }

    fn finish_legacy_migration(&self) {
        self.legacy_migration_done.store(true, Ordering::Release);
        self.legacy_migration_notify.notify_waiters();
    }

    pub async fn migrate_legacy_paths(&self, state: &AppState, data_dir: &Path) {
        if !self.legacy_migration_pending {
            return;
        }
        let mut files = Vec::new();
        for key in ["open_tabs", "development_open_tabs"] {
            if let Ok(Some(value)) = state.storage.load_open_tabs_state_with_key(key).await {
                collect_external_sql_paths(&value, &mut files);
            }
        }
        for file in &files {
            let _ = self.grant_file(Path::new(file));
        }
        if let Ok(bytes) = std::fs::read(data_dir.join("global-search-settings.json")) {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                for root in value.get("roots").and_then(|roots| roots.as_array()).into_iter().flatten() {
                    if let Some(root) = root.as_str() {
                        let _ = self.grant_directory(Path::new(root));
                    }
                }
            }
        }
        // Always write the file so the migration never runs again.
        {
            let grants = self.lock();
            self.persist(&grants);
        }
        self.finish_legacy_migration();
    }
}

fn collect_external_sql_paths(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key == "externalSqlPath" {
                    if let Some(path) = value.as_str() {
                        out.push(path.to_string());
                    }
                } else {
                    collect_external_sql_paths(value, out);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect_external_sql_paths(item, out)),
        _ => {}
    }
}

/// Registers the grant store and schedules the one-time legacy migration.
pub fn install<R: Runtime>(app: &AppHandle<R>, data_dir: &Path, state: Arc<AppState>) {
    let access = Arc::new(ExternalPathAccess::load(data_dir));
    app.manage(access.clone());
    let data_dir = data_dir.to_path_buf();
    // Storage may still be unmigrated behind the startup migration wizard;
    // read the legacy tab state only once it is ready.
    let gate = app.try_state::<Arc<crate::migration_gate::MigrationGate>>().map(|gate| gate.inner().clone());
    tauri::async_runtime::spawn(async move {
        if let Some(gate) = gate {
            gate.wait().await;
        }
        access.migrate_legacy_paths(&state, &data_dir).await;
    });
}

fn access_state<R: Runtime>(app: &AppHandle<R>) -> Result<Arc<ExternalPathAccess>, String> {
    app.try_state::<Arc<ExternalPathAccess>>()
        .map(|state| state.inner().clone())
        .ok_or_else(|| "External path authorization is unavailable".to_string())
}

/// Grants files handed over by the OS (launch args, open-file events).
pub fn grant_opened_files<R: Runtime>(app: &AppHandle<R>, paths: &[String]) {
    if let Ok(access) = access_state(app) {
        for path in paths {
            let _ = access.grant_file(Path::new(path));
        }
    }
}

/// Grants files dropped onto a DBX window. Directories are not granted.
pub fn grant_dropped_paths<R: Runtime>(app: &AppHandle<R>, paths: &[PathBuf]) {
    if let Ok(access) = access_state(app) {
        for path in paths.iter().filter(|path| path.is_file()) {
            let _ = access.grant_file(path);
        }
    }
}

pub fn grant_file<R: Runtime>(app: &AppHandle<R>, path: &Path) -> bool {
    access_state(app).map(|access| access.grant_file(path)).unwrap_or(false)
}

pub fn grant_session_file<R: Runtime>(app: &AppHandle<R>, path: &Path) {
    if let Ok(access) = access_state(app) {
        access.grant_session_file(path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    Read,
    Write,
}

fn not_authorized(path: &Path) -> String {
    format!("{NOT_AUTHORIZED_ERROR_PREFIX}: DBX has not been given access to {}", path.display())
}

fn prompt_text(
    paths: &[PathBuf],
    directory: bool,
    kind: AccessKind,
    requester: Option<&str>,
) -> (String, String, String) {
    let zh = sys_locale::get_locale().unwrap_or_default().to_ascii_lowercase().starts_with("zh");
    let mut listed =
        paths.iter().take(MAX_PROMPT_LISTED_PATHS).map(|path| path.display().to_string()).collect::<Vec<_>>();
    if paths.len() > MAX_PROMPT_LISTED_PATHS {
        listed.push(if zh {
            format!("……以及另外 {} 项", paths.len() - MAX_PROMPT_LISTED_PATHS)
        } else {
            format!("...and {} more", paths.len() - MAX_PROMPT_LISTED_PATHS)
        });
    }
    let listed = listed.join("\n");
    let body = match (zh, directory, kind, requester) {
        (true, _, _, Some(requester)) => format!(
            "插件“{requester}”请求{}以下文件：\n\n{listed}\n\n仅在你刚刚选择了该文件时才允许。",
            if kind == AccessKind::Write { "写入" } else { "读取" }
        ),
        (true, true, _, None) => format!("是否允许 DBX 访问以下文件夹及其内容？\n\n{listed}"),
        (true, false, AccessKind::Write, None) => format!("是否允许 DBX 写入以下文件？\n\n{listed}"),
        (true, false, AccessKind::Read, None) => format!("是否允许 DBX 读取以下文件？\n\n{listed}"),
        (false, _, _, Some(requester)) => format!(
            "The plugin \"{requester}\" wants to {} this file:\n\n{listed}\n\nAllow only if you just chose this file.",
            if kind == AccessKind::Write { "write" } else { "read" }
        ),
        (false, true, _, None) => format!("Allow DBX to access these folders and their contents?\n\n{listed}"),
        (false, false, AccessKind::Write, None) => format!("Allow DBX to write to this file?\n\n{listed}"),
        (false, false, AccessKind::Read, None) => format!("Allow DBX to read this file?\n\n{listed}"),
    };
    let (allow, cancel) = if zh { ("允许", "取消") } else { ("Allow", "Cancel") };
    (body, allow.to_string(), cancel.to_string())
}

async fn confirm_access<R: Runtime>(
    app: &AppHandle<R>,
    paths: &[PathBuf],
    directory: bool,
    kind: AccessKind,
    requester: Option<&str>,
) -> bool {
    let (body, allow, cancel) = prompt_text(paths, directory, kind, requester);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .message(body)
        .title("DBX")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(allow, cancel))
        .show(move |approved| {
            let _ = sender.send(approved);
        });
    receiver.await.unwrap_or(false)
}

/// Checks a file path, optionally asking the user through a native dialog when
/// it has not been granted yet. Returns the normalized path.
pub async fn ensure_file_access<R: Runtime>(
    app: &AppHandle<R>,
    path: &Path,
    kind: AccessKind,
    prompt: bool,
    requester: Option<&str>,
) -> Result<PathBuf, String> {
    let access = access_state(app)?;
    access.wait_for_legacy_migration().await;
    if let Some(normalized) = access.allowed_file(path) {
        return Ok(normalized);
    }
    let Some(normalized) = normalize_path(path) else { return Err(not_authorized(path)) };
    if !prompt || access.is_forbidden(&normalized) || normalized.is_dir() {
        return Err(not_authorized(path));
    }
    let _prompt = access.prompt_lock.lock().await;
    if let Some(normalized) = access.allowed_file(path) {
        return Ok(normalized);
    }
    if confirm_access(app, std::slice::from_ref(&normalized), false, kind, requester).await
        && access.grant_file(&normalized)
    {
        return Ok(normalized);
    }
    Err(not_authorized(path))
}

/// Checks a directory path (no prompt). Returns the normalized directory.
pub async fn ensure_directory_access<R: Runtime>(app: &AppHandle<R>, path: &Path) -> Result<PathBuf, String> {
    let access = access_state(app)?;
    access.wait_for_legacy_migration().await;
    access.allowed_directory(path).ok_or_else(|| not_authorized(path))
}

/// Asks the user once (native dialog) to allow access to paths the frontend
/// remembers, e.g. SQL folders saved before access grants existed. Paths that
/// are already granted do not prompt. Returns whether all paths are allowed.
#[tauri::command]
pub async fn request_external_path_access(app: AppHandle, paths: Vec<String>, directory: bool) -> Result<bool, String> {
    let access = access_state(&app)?;
    access.wait_for_legacy_migration().await;
    let _prompt = access.prompt_lock.lock().await;
    let pending = paths
        .iter()
        .map(PathBuf::from)
        .filter(|path| !access.is_allowed(path, directory))
        .take(MAX_PROMPT_PATHS + 1)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(true);
    }
    if pending.len() > MAX_PROMPT_PATHS {
        return Err("Too many paths in one access request".to_string());
    }
    let normalized = pending.iter().filter_map(|path| normalize_path(path)).collect::<Vec<_>>();
    if normalized.len() != pending.len() || normalized.iter().any(|path| access.is_forbidden(path)) {
        return Ok(false);
    }
    if !confirm_access(&app, &normalized, directory, AccessKind::Read, None).await {
        return Ok(false);
    }
    // Grant every path (no short-circuit), then report whether all succeeded.
    let results: Vec<bool> = normalized
        .iter()
        .map(|path| if directory { access.grant_directory(path) } else { access.grant_file(path) })
        .collect();
    Ok(results.into_iter().all(|granted| granted))
}

/// Native file picker whose selection is granted to the external file commands.
#[tauri::command]
pub async fn pick_external_files(
    window: tauri::Window,
    multiple: Option<bool>,
    filter_name: Option<String>,
    extensions: Option<Vec<String>>,
    title: Option<String>,
) -> Result<Vec<String>, String> {
    let access = access_state(window.app_handle())?;
    let mut dialog = window.dialog().file();
    if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
        dialog = dialog.set_title(title);
    }
    let extensions = extensions
        .unwrap_or_default()
        .into_iter()
        .map(|extension| extension.trim().trim_start_matches('.').to_string())
        .filter(|extension| !extension.is_empty() && extension.chars().all(|c| c.is_ascii_alphanumeric()))
        .collect::<Vec<_>>();
    if !extensions.is_empty() {
        let refs = extensions.iter().map(String::as_str).collect::<Vec<_>>();
        dialog = dialog.add_filter(filter_name.unwrap_or_else(|| "Files".to_string()), &refs);
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    if multiple == Some(true) {
        dialog.pick_files(move |paths| {
            let _ = sender.send(paths.unwrap_or_default());
        });
    } else {
        dialog.pick_file(move |path| {
            let _ = sender.send(path.into_iter().collect());
        });
    }
    let picked = receiver.await.map_err(|_| "File dialog closed unexpectedly".to_string())?;
    let mut paths = Vec::with_capacity(picked.len());
    for file_path in picked {
        let path = file_path.into_path().map_err(|error| format!("Failed to resolve file path: {error}"))?;
        if access.grant_file(&path) {
            paths.push(path.to_string_lossy().into_owned());
        }
    }
    Ok(paths)
}

/// Native save dialog whose chosen path is granted (for hosts that write the
/// file through an authorized command afterwards, e.g. plugin file saves).
#[tauri::command]
pub async fn pick_external_save_path(
    window: tauri::Window,
    default_file_name: Option<String>,
    extension: Option<String>,
) -> Result<Option<String>, String> {
    let access = access_state(window.app_handle())?;
    let mut dialog = window.dialog().file();
    if let Some(name) = default_file_name.filter(|name| !name.trim().is_empty()) {
        dialog = dialog.set_file_name(name);
    }
    if let Some(extension) = extension
        .map(|extension| extension.trim().trim_start_matches('.').to_string())
        .filter(|extension| !extension.is_empty() && extension.chars().all(|c| c.is_ascii_alphanumeric()))
    {
        dialog = dialog.add_filter(extension.to_uppercase(), &[extension.as_str()]);
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    dialog.save_file(move |path| {
        let _ = sender.send(path);
    });
    let Some(file_path) = receiver.await.map_err(|_| "Save dialog closed unexpectedly".to_string())? else {
        return Ok(None);
    };
    let path = file_path.into_path().map_err(|error| format!("Failed to resolve file path: {error}"))?;
    if !access.grant_file(&path) {
        return Err(format!("{NOT_AUTHORIZED_ERROR_PREFIX}: this location cannot be used: {}", path.display()));
    }
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// Native folder picker whose selection is granted recursively.
#[tauri::command]
pub async fn pick_external_directory(window: tauri::Window, title: Option<String>) -> Result<Option<String>, String> {
    let access = access_state(window.app_handle())?;
    let mut dialog = window.dialog().file();
    if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
        dialog = dialog.set_title(title);
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    dialog.pick_folder(move |path| {
        let _ = sender.send(path);
    });
    let Some(file_path) = receiver.await.map_err(|_| "Folder dialog closed unexpectedly".to_string())? else {
        return Ok(None);
    };
    let path = file_path.into_path().map_err(|error| format!("Failed to resolve folder path: {error}"))?;
    if !access.grant_directory(&path) {
        return Err(format!("{NOT_AUTHORIZED_ERROR_PREFIX}: this folder cannot be used: {}", path.display()));
    }
    Ok(Some(path.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dbx-path-access-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::canonicalize(root).unwrap()
    }

    #[test]
    fn ungranted_paths_are_rejected_and_file_grants_are_exact() {
        let root = temp_root("file");
        let granted = root.join("a.sql");
        let other = root.join("b.sql");
        std::fs::write(&granted, "select 1").unwrap();
        std::fs::write(&other, "select 2").unwrap();
        let access = ExternalPathAccess::in_memory(Vec::new());

        assert!(access.allowed_file(&granted).is_none());
        assert!(access.grant_file(&granted));
        assert_eq!(access.allowed_file(&granted), Some(granted.clone()));
        assert!(access.allowed_file(&other).is_none());
        assert!(access.allowed_file(Path::new("relative.sql")).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn directory_grants_are_recursive_and_block_traversal() {
        let root = temp_root("dir");
        let granted = root.join("granted");
        std::fs::create_dir_all(granted.join("nested")).unwrap();
        std::fs::write(granted.join("nested").join("q.sql"), "select 1").unwrap();
        std::fs::write(root.join("outside.sql"), "select 2").unwrap();
        let access = ExternalPathAccess::in_memory(Vec::new());
        assert!(access.grant_directory(&granted));

        assert!(access.allowed_file(&granted.join("nested").join("q.sql")).is_some());
        assert!(access.allowed_file(&granted.join("new.sql")).is_some(), "missing files resolve via parent");
        assert!(access.allowed_file(&granted.join("..").join("outside.sql")).is_none());
        assert!(access.allowed_directory(&granted.join("nested")).is_some());
        assert!(access.allowed_directory(&root).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn data_directory_is_never_granted() {
        let root = temp_root("forbidden");
        let data_dir = root.join("data");
        std::fs::create_dir_all(data_dir.join("plugins")).unwrap();
        std::fs::write(data_dir.join("plugins").join("x.js"), "").unwrap();
        let access = ExternalPathAccess::in_memory(vec![data_dir.clone()]);

        assert!(!access.grant_file(&data_dir.join("plugins").join("x.js")));
        assert!(!access.grant_directory(&data_dir));
        assert!(access.grant_directory(&root));
        assert!(access.allowed_file(&data_dir.join("plugins").join("x.js")).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn grants_persist_and_reload() {
        let root = temp_root("persist");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let file = root.join("keep.sql");
        std::fs::write(&file, "select 1").unwrap();

        let access = ExternalPathAccess::load(&data_dir);
        assert!(access.legacy_migration_pending);
        assert!(access.grant_file(&file));
        access.grant_session_file(&root.join("session.sql"));

        let reloaded = ExternalPathAccess::load(&data_dir);
        assert!(!reloaded.legacy_migration_pending);
        assert!(reloaded.allowed_file(&file).is_some());
        assert!(reloaded.allowed_file(&root.join("session.sql")).is_none(), "session grants are not persisted");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collects_external_sql_paths_from_open_tabs_payload() {
        let payload = serde_json::json!({
            "tabs": [
                { "id": "1", "externalSqlPath": "/tmp/a.sql" },
                { "id": "2" },
                { "id": "3", "nested": { "externalSqlPath": "/tmp/b.sql" } }
            ]
        });
        let mut paths = Vec::new();
        collect_external_sql_paths(&payload, &mut paths);
        assert_eq!(paths, vec!["/tmp/a.sql", "/tmp/b.sql"]);
    }

    #[test]
    fn external_text_extensions_exclude_secrets_and_binaries() {
        assert!(has_external_text_file_extension(Path::new("/tmp/a.SQL")));
        assert!(has_external_text_file_extension(Path::new("/tmp/run.sh")));
        assert!(!has_external_text_file_extension(Path::new("/home/u/.ssh/id_rsa")));
        assert!(!has_external_text_file_extension(Path::new("/home/u/.zshrc")));
        assert!(!has_external_text_file_extension(Path::new("/tmp/key.pem")));
        assert!(!has_external_text_file_extension(Path::new("/tmp/app.db")));
        assert!(!has_external_text_file_extension(Path::new("/tmp/Info.plist")));
    }
}
