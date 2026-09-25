//! Client-facing redaction of AI provider configurations.
//!
//! API keys, gateway headers, credentialed proxy URLs and CLI environment
//! values stay in the backend. Configurations sent to a UI have those values
//! blanked and list the stored ones in `savedSecrets`; configurations sent
//! back (save, test, list models, chat requests) have blank values filled from
//! the stored configuration with the same id unless the UI listed the path in
//! `clearedSecrets`.

use std::collections::HashSet;

use reqwest::Url;

use crate::ai::{AiConfig, AiConfigItem};

/// Field added to redacted AI configurations sent to a client.
pub const AI_CLIENT_SAVED_SECRETS_FIELD: &str = "savedSecrets";
/// Optional field of a client AI configuration: secret paths to remove.
pub const AI_CLIENT_CLEARED_SECRETS_FIELD: &str = "clearedSecrets";
/// Optional field of a client AI config item created from a legacy
/// configuration: `"legacy"` (single active config) or `"provider:<key>"`.
/// Its stored secrets fill blank values when the item id is not stored yet.
pub const AI_CLIENT_LEGACY_SECRETS_FIELD: &str = "legacySecretsFrom";

/// Map-valued secret fields: names stay visible, values are secret.
const AI_MAP_SECRET_FIELDS: &[&str] = &[
    "customHeaders",
    "codexCliEnv",
    "claudeCodeCliEnv",
    "piAgentCliEnv",
    "opencodeCliEnv",
    "cursorCliEnv",
    "grokCliEnv",
    "codebuddyCliEnv",
    "qoderCliEnv",
];

/// Whether a proxy URL carries credentials (`scheme://user:pass@host`).
pub fn proxy_url_has_credentials(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    match Url::parse(value) {
        Ok(url) => !url.username().is_empty() || url.password().is_some(),
        // Unparseable values are hidden when they could contain userinfo.
        Err(_) => value.contains('@'),
    }
}

fn redact_ai_object(object: &mut serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut saved = Vec::new();
    if object.get("apiKey").and_then(serde_json::Value::as_str).is_some_and(|key| !key.is_empty()) {
        saved.push("apiKey".to_string());
    }
    if object.contains_key("apiKey") {
        object.insert("apiKey".to_string(), serde_json::Value::String(String::new()));
    }
    if object.get("proxyUrl").and_then(serde_json::Value::as_str).is_some_and(proxy_url_has_credentials) {
        saved.push("proxyUrl".to_string());
        object.insert("proxyUrl".to_string(), serde_json::Value::String(String::new()));
    }
    for field in AI_MAP_SECRET_FIELDS {
        let Some(map) = object.get_mut(*field).and_then(serde_json::Value::as_object_mut) else {
            continue;
        };
        let mut names = map.keys().cloned().collect::<Vec<_>>();
        names.sort();
        for name in names {
            if map.get(&name).and_then(serde_json::Value::as_str).is_some_and(|value| !value.is_empty()) {
                saved.push(format!("{field}.{name}"));
            }
            map.insert(name, serde_json::Value::String(String::new()));
        }
    }
    saved
}

fn insert_saved(object: &mut serde_json::Map<String, serde_json::Value>, saved: Vec<String>) {
    object.insert(
        AI_CLIENT_SAVED_SECRETS_FIELD.to_string(),
        serde_json::Value::Array(saved.into_iter().map(serde_json::Value::String).collect()),
    );
}

pub fn redact_ai_config_for_client(config: &AiConfig) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(config).map_err(|error| error.to_string())?;
    let object = value.as_object_mut().ok_or_else(|| "AI config must serialize to an object".to_string())?;
    let saved = redact_ai_object(object);
    insert_saved(object, saved);
    Ok(value)
}

pub fn redact_ai_config_item_for_client(item: &AiConfigItem) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(item).map_err(|error| error.to_string())?;
    let object = value.as_object_mut().ok_or_else(|| "AI config must serialize to an object".to_string())?;
    let saved = redact_ai_object(object);
    insert_saved(object, saved);
    Ok(value)
}

pub fn redact_ai_config_items_for_client(items: &[AiConfigItem]) -> Result<Vec<serde_json::Value>, String> {
    items.iter().map(redact_ai_config_item_for_client).collect()
}

fn take_cleared(value: &mut serde_json::Value) -> Result<Vec<String>, String> {
    let Some(object) = value.as_object_mut() else {
        return Err("AI config must be a JSON object".to_string());
    };
    object.remove(AI_CLIENT_SAVED_SECRETS_FIELD);
    match object.remove(AI_CLIENT_CLEARED_SECRETS_FIELD) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value).map_err(|error| format!("Invalid clearedSecrets: {error}")),
    }
}

/// An AI config item received from a client together with its cleared paths.
#[derive(Debug, Clone)]
pub struct ClientAiConfigItem {
    pub item: AiConfigItem,
    pub cleared_secrets: Vec<String>,
    pub legacy_secrets_from: Option<String>,
}

impl<'de> serde::Deserialize<'de> for ClientAiConfigItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
        let cleared_secrets = take_cleared(&mut value).map_err(serde::de::Error::custom)?;
        let legacy_secrets_from = value
            .as_object_mut()
            .and_then(|object| object.remove(AI_CLIENT_LEGACY_SECRETS_FIELD))
            .and_then(|value| value.as_str().map(str::to_string))
            .filter(|source| !source.trim().is_empty());
        let item = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self { item, cleared_secrets, legacy_secrets_from })
    }
}

impl From<AiConfigItem> for ClientAiConfigItem {
    fn from(item: AiConfigItem) -> Self {
        Self { item, cleared_secrets: Vec::new(), legacy_secrets_from: None }
    }
}

/// An AI config received from a client together with its cleared paths.
#[derive(Debug, Clone)]
pub struct ClientAiConfig {
    pub config: AiConfig,
    pub cleared_secrets: Vec<String>,
}

impl<'de> serde::Deserialize<'de> for ClientAiConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
        let cleared_secrets = take_cleared(&mut value).map_err(serde::de::Error::custom)?;
        let config = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self { config, cleared_secrets })
    }
}

impl From<AiConfig> for ClientAiConfig {
    fn from(config: AiConfig) -> Self {
        Self { config, cleared_secrets: Vec::new() }
    }
}

/// How blank values of a client AI configuration are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiSecretMergeMode {
    /// Saving: a header/env name missing from the client map was deleted.
    Save,
    /// Running a request: missing names are filled from the stored config.
    Request,
}

/// Fills blank secret values of `incoming` from `stored`. Non-empty client
/// values always win; `cleared` paths stay blank (map entries are removed).
pub fn merge_stored_ai_secrets(
    incoming: &AiConfig,
    stored: Option<&AiConfig>,
    cleared: &[String],
    mode: AiSecretMergeMode,
) -> Result<AiConfig, String> {
    let cleared = cleared.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut value = serde_json::to_value(incoming).map_err(|error| error.to_string())?;
    let stored_value = stored.map(serde_json::to_value).transpose().map_err(|error| error.to_string())?;
    let stored_object = stored_value.as_ref().and_then(serde_json::Value::as_object);
    let object = value.as_object_mut().ok_or_else(|| "AI config must serialize to an object".to_string())?;

    let stored_string = |field: &str| {
        stored_object
            .and_then(|stored| stored.get(field))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let incoming_blank = |object: &serde_json::Map<String, serde_json::Value>, field: &str| {
        object.get(field).and_then(serde_json::Value::as_str).is_none_or(str::is_empty)
    };

    if cleared.contains("apiKey") {
        object.insert("apiKey".to_string(), serde_json::Value::String(String::new()));
    } else if incoming_blank(object, "apiKey") {
        if let Some(secret) = stored_string("apiKey") {
            object.insert("apiKey".to_string(), serde_json::Value::String(secret));
        }
    }

    if cleared.contains("proxyUrl") {
        object.insert("proxyUrl".to_string(), serde_json::Value::String(String::new()));
    } else if incoming_blank(object, "proxyUrl") {
        // A proxy URL without credentials is shown to the client, so a blank
        // value there is an edit; only a hidden (credentialed) URL is kept.
        if let Some(secret) = stored_string("proxyUrl").filter(|url| proxy_url_has_credentials(url)) {
            object.insert("proxyUrl".to_string(), serde_json::Value::String(secret));
        }
    }

    for field in AI_MAP_SECRET_FIELDS {
        let stored_map = stored_object.and_then(|stored| stored.get(*field)).and_then(serde_json::Value::as_object);
        let mut map = object.get(*field).and_then(serde_json::Value::as_object).cloned().unwrap_or_default();
        let mut names = map.keys().cloned().collect::<Vec<_>>();
        if mode == AiSecretMergeMode::Request {
            if let Some(stored_map) = stored_map {
                for name in stored_map.keys() {
                    if !map.contains_key(name) {
                        names.push(name.clone());
                    }
                }
            }
        }
        for name in names {
            let path = format!("{field}.{name}");
            let current_blank = map.get(&name).and_then(serde_json::Value::as_str).is_none_or(str::is_empty);
            if !current_blank {
                continue;
            }
            if cleared.contains(path.as_str()) {
                map.remove(&name);
                continue;
            }
            match stored_map.and_then(|stored| stored.get(&name)).and_then(serde_json::Value::as_str) {
                Some(secret) if !secret.is_empty() => {
                    map.insert(name, serde_json::Value::String(secret.to_string()));
                }
                // A blank value with nothing stored carries no information.
                _ => {
                    map.remove(&name);
                }
            }
        }
        object.insert((*field).to_string(), serde_json::Value::Object(map));
    }

    serde_json::from_value(value).map_err(|error| error.to_string())
}

impl crate::storage::Storage {
    /// Fills blank secrets of an AI configuration sent with a request from
    /// the stored configuration `config_id`. Without an id the configuration
    /// is used as sent (for example a new, unsaved provider being tested).
    pub async fn resolve_client_ai_config(
        &self,
        config: &AiConfig,
        config_id: Option<&str>,
    ) -> Result<AiConfig, String> {
        let Some(config_id) = config_id.map(str::trim).filter(|id| !id.is_empty()) else {
            return Ok(config.clone());
        };
        let stored = self.load_ai_configs().await?.into_iter().find(|item| item.id == config_id);
        merge_stored_ai_secrets(config, stored.as_ref().map(|item| &item.config), &[], AiSecretMergeMode::Request)
    }

    /// Replaces the AI configuration list from a client, keeping stored
    /// secrets the client left blank.
    pub async fn save_client_ai_configs(&self, items: &[ClientAiConfigItem]) -> Result<(), String> {
        let stored = self.load_ai_configs().await?;
        let needs_legacy = items.iter().any(|input| input.legacy_secrets_from.is_some());
        let legacy_active = if needs_legacy { self.load_ai_config().await? } else { None };
        let legacy_providers = if needs_legacy { self.load_ai_provider_configs().await? } else { Default::default() };
        let mut merged = Vec::with_capacity(items.len());
        for input in items {
            let legacy = input.legacy_secrets_from.as_deref().and_then(|source| match source {
                "legacy" => legacy_active.as_ref(),
                source => source.strip_prefix("provider:").and_then(|provider| legacy_providers.get(provider)),
            });
            let previous =
                stored.iter().find(|item| item.id == input.item.id).map(|item| &item.config).or(legacy);
            let config =
                merge_stored_ai_secrets(&input.item.config, previous, &input.cleared_secrets, AiSecretMergeMode::Save)?;
            merged.push(AiConfigItem { config, ..input.item.clone() });
        }
        self.save_ai_configs(&merged).await
    }

    pub async fn save_client_ai_config_item(&self, input: &ClientAiConfigItem) -> Result<(), String> {
        let stored = self.load_ai_configs().await?.into_iter().find(|item| item.id == input.item.id);
        let config = merge_stored_ai_secrets(
            &input.item.config,
            stored.as_ref().map(|item| &item.config),
            &input.cleared_secrets,
            AiSecretMergeMode::Save,
        )?;
        self.save_ai_config_item(&AiConfigItem { config, ..input.item.clone() }).await
    }

    /// Legacy single-configuration save.
    pub async fn save_client_ai_config(&self, input: &ClientAiConfig) -> Result<(), String> {
        let stored = self.load_ai_config().await?;
        let config =
            merge_stored_ai_secrets(&input.config, stored.as_ref(), &input.cleared_secrets, AiSecretMergeMode::Save)?;
        self.save_ai_config(&config).await
    }

    /// Legacy per-provider save.
    pub async fn save_client_ai_provider_config(&self, provider: &str, input: &ClientAiConfig) -> Result<(), String> {
        let stored = self.load_ai_provider_configs().await?.remove(provider);
        let config =
            merge_stored_ai_secrets(&input.config, stored.as_ref(), &input.cleared_secrets, AiSecretMergeMode::Save)?;
        self.save_ai_provider_config(provider, &config).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(api_key: &str) -> AiConfig {
        serde_json::from_value(serde_json::json!({
            "provider": "openai",
            "apiKey": api_key,
            "endpoint": "https://api.example.com",
            "model": "m",
            "customHeaders": { "X-Gateway-Key": "header-secret" },
            "proxyUrl": "http://user:proxy-secret@proxy.local:8080",
            "codexCliEnv": { "OPENAI_API_KEY": "env-secret" }
        }))
        .unwrap()
    }

    #[test]
    fn redaction_hides_keys_headers_env_and_credentialed_proxy() {
        let value = redact_ai_config_for_client(&config("sk-secret")).unwrap();
        let text = value.to_string();
        for secret in ["sk-secret", "header-secret", "proxy-secret", "env-secret"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert_eq!(value["customHeaders"]["X-Gateway-Key"], "");
        assert_eq!(
            value[AI_CLIENT_SAVED_SECRETS_FIELD],
            serde_json::json!(["apiKey", "proxyUrl", "customHeaders.X-Gateway-Key", "codexCliEnv.OPENAI_API_KEY"])
        );
    }

    #[test]
    fn plain_proxy_url_stays_visible_and_blank_means_removed() {
        let mut stored = config("sk");
        stored.proxy_url = "http://proxy.local:8080".to_string();
        let value = redact_ai_config_for_client(&stored).unwrap();
        assert_eq!(value["proxyUrl"], "http://proxy.local:8080");
        let mut incoming = stored.clone();
        incoming.proxy_url.clear();
        let merged = merge_stored_ai_secrets(&incoming, Some(&stored), &[], AiSecretMergeMode::Save).unwrap();
        assert_eq!(merged.proxy_url, "");
    }

    #[test]
    fn merge_restores_blank_values_and_honors_clears_and_removals() {
        let stored = config("sk-secret");
        let redacted: ClientAiConfig = serde_json::from_value(redact_ai_config_for_client(&stored).unwrap()).unwrap();
        let merged = merge_stored_ai_secrets(&redacted.config, Some(&stored), &[], AiSecretMergeMode::Save).unwrap();
        assert_eq!(merged.api_key, "sk-secret");
        assert_eq!(merged.proxy_url, "http://user:proxy-secret@proxy.local:8080");
        assert_eq!(merged.custom_headers.get("X-Gateway-Key").map(String::as_str), Some("header-secret"));
        assert_eq!(merged.codex_cli_env.get("OPENAI_API_KEY").map(String::as_str), Some("env-secret"));

        let mut edited = redacted.config.clone();
        edited.custom_headers.clear();
        let cleared = vec!["apiKey".to_string(), "codexCliEnv.OPENAI_API_KEY".to_string()];
        let merged = merge_stored_ai_secrets(&edited, Some(&stored), &cleared, AiSecretMergeMode::Save).unwrap();
        assert_eq!(merged.api_key, "");
        assert!(merged.custom_headers.is_empty());
        assert!(merged.codex_cli_env.is_empty());

        let merged = merge_stored_ai_secrets(&edited, Some(&stored), &[], AiSecretMergeMode::Request).unwrap();
        assert_eq!(merged.custom_headers.get("X-Gateway-Key").map(String::as_str), Some("header-secret"));
    }

    #[test]
    fn typed_values_replace_stored_values() {
        let stored = config("sk-old");
        let incoming = config("sk-new");
        let merged = merge_stored_ai_secrets(&incoming, Some(&stored), &[], AiSecretMergeMode::Save).unwrap();
        assert_eq!(merged.api_key, "sk-new");
    }

    #[tokio::test]
    async fn saving_a_redacted_item_keeps_stored_secrets_and_requests_resolve_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::storage::Storage::open(&dir.path().join("dbx.db")).await.unwrap();
        let item = AiConfigItem { id: "cfg".to_string(), name: "Main".to_string(), is_default: true, config: config("sk-secret") };
        storage.save_ai_configs(&[item.clone()]).await.unwrap();

        let redacted = redact_ai_config_item_for_client(&storage.load_ai_configs().await.unwrap()[0]).unwrap();
        assert!(!redacted.to_string().contains("sk-secret"));
        let mut input: ClientAiConfigItem = serde_json::from_value(redacted).unwrap();
        input.item.name = "Renamed".to_string();
        storage.save_client_ai_config_item(&input).await.unwrap();
        let saved = storage.load_ai_configs().await.unwrap();
        assert_eq!(saved[0].name, "Renamed");
        assert_eq!(saved[0].config.api_key, "sk-secret");

        let resolved = storage.resolve_client_ai_config(&input.item.config, Some("cfg")).await.unwrap();
        assert_eq!(resolved.api_key, "sk-secret");
        let unresolved = storage.resolve_client_ai_config(&input.item.config, None).await.unwrap();
        assert_eq!(unresolved.api_key, "");

        input.cleared_secrets = vec!["apiKey".to_string()];
        storage.save_client_ai_configs(&[input]).await.unwrap();
        assert_eq!(storage.load_ai_configs().await.unwrap()[0].config.api_key, "");
    }
}
