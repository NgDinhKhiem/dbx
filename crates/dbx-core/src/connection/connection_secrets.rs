use crate::models::connection::{ConnectionConfig, DatabaseType, TransportLayerConfig};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

pub const MAIN_PASSWORD_KEY: &str = "password";
pub const SSH_PASSWORD_KEY: &str = "ssh_password";
pub const SSH_KEY_PASSPHRASE_KEY: &str = "ssh_key_passphrase";
pub const SSH_TUNNEL_SECRET_PREFIX: &str = "ssh_tunnels.";
pub const TRANSPORT_LAYER_SECRET_PREFIX: &str = "transport_layers.";
pub const PROXY_PASSWORD_KEY: &str = "proxy_password";
pub const REDIS_SENTINEL_PASSWORD_KEY: &str = "redis_sentinel_password";
pub const CONNECTION_STRING_KEY: &str = "connection_string";
pub const INIT_SCRIPT_KEY: &str = "init_script";
pub const MQ_AUTH_SECRET_PREFIX: &str = "mq.auth.";
pub const MQ_AUTH_TOKEN_KEY: &str = "mq.auth.token";
pub const MQ_AUTH_PASSWORD_KEY: &str = "mq.auth.password";
pub const MQ_AUTH_API_KEY_VALUE_KEY: &str = "mq.auth.api_key_value";
pub const MQ_AUTH_CLIENT_SECRET_KEY: &str = "mq.auth.client_secret";
pub const MQ_TOKEN_SIGNING_SECRET_PREFIX: &str = "mq.token_signing.";
pub const MQ_TOKEN_SIGNING_KEY: &str = "mq.token_signing.key";
pub const NACOS_AUTH_SECRET_PREFIX: &str = "nacos.auth.";
pub const NACOS_AUTH_PASSWORD_KEY: &str = "nacos.auth.password";
pub const NACOS_RNACOS_CONSOLE_PASSWORD_KEY: &str = "nacos.auth.rnacos_console_password";
pub const MQTT_AUTH_SECRET_PREFIX: &str = "mqtt.auth.";
pub const MQTT_AUTH_PASSWORD_KEY: &str = "mqtt.auth.password";
pub const CASSANDRA_TLS_SECRET_PREFIX: &str = "cassandra.tls.";
pub const CASSANDRA_TRUSTSTORE_PASSWORD_KEY: &str = "cassandra.tls.truststore_password";
pub const CASSANDRA_KEYSTORE_PASSWORD_KEY: &str = "cassandra.tls.keystore_password";
pub const PLUGIN_CONNECTION_SECRET_PREFIX: &str = "plugin_connection.";

/// Storage-level secret key for one plugin connection field: namespaced under
/// [`PLUGIN_CONNECTION_SECRET_PREFIX`] so plugin keys never collide with the
/// fixed driver keys and can be scrubbed with a single prefix delete.
pub fn plugin_connection_secret_key(key: &str) -> Result<String, String> {
    if key.is_empty() {
        return Err("Plugin connection secret key must not be empty".to_string());
    }
    Ok(format!("{PLUGIN_CONNECTION_SECRET_PREFIX}{key}"))
}

pub trait ConnectionSecretStore {
    fn set_secret(&self, connection_id: &str, key: &str, secret: &str) -> Result<(), String>;
    fn get_secret(&self, connection_id: &str, key: &str) -> Result<Option<String>, String>;
    fn delete_secret(&self, connection_id: &str, key: &str) -> Result<(), String>;
    fn delete_secret_prefix(&self, _connection_id: &str, _key_prefix: &str) -> Result<(), String> {
        Ok(())
    }
}

pub fn save_connections_to_file(
    path: &Path,
    configs: &[ConnectionConfig],
    store: &dyn ConnectionSecretStore,
) -> Result<(), String> {
    delete_removed_connection_secrets(path, configs, store)?;
    for config in configs {
        persist_secret(store, &config.id, MAIN_PASSWORD_KEY, &config.password)?;
        delete_secret_prefix(store, &config.id, TRANSPORT_LAYER_SECRET_PREFIX)?;
        for (index, layer) in config.transport_layers.iter().enumerate() {
            persist_transport_layer_secrets(store, &config.id, index, layer)?;
        }
        persist_secret(store, &config.id, REDIS_SENTINEL_PASSWORD_KEY, &config.redis_sentinel_password)?;
        persist_optional_secret(store, &config.id, CONNECTION_STRING_KEY, config.connection_string.as_deref())?;
        persist_optional_secret(store, &config.id, INIT_SCRIPT_KEY, config.init_script.as_deref())?;
        persist_mq_auth_secrets(store, config)?;
        persist_mq_token_signing_secret(store, config)?;
        persist_mqtt_auth_secrets(store, config)?;
        persist_cassandra_tls_secrets(store, config)?;
        delete_secret_prefix(store, &config.id, PLUGIN_CONNECTION_SECRET_PREFIX)?;
        for (key, secret) in &config.connection_secrets {
            persist_secret(store, &config.id, &plugin_connection_secret_key(key)?, secret)?;
        }

        // New configs persist transport-layer secrets only. Remove legacy transport secret slots after the
        // migrated layer values have been written so old configs do not keep two sources of truth.
        store.delete_secret(&config.id, SSH_PASSWORD_KEY)?;
        store.delete_secret(&config.id, SSH_KEY_PASSPHRASE_KEY)?;
        store.delete_secret(&config.id, PROXY_PASSWORD_KEY)?;
        delete_secret_prefix(store, &config.id, SSH_TUNNEL_SECRET_PREFIX)?;
    }

    write_sanitized_connections(path, configs)
}

pub fn load_connections_from_file(
    path: &Path,
    store: &dyn ConnectionSecretStore,
) -> Result<Vec<ConnectionConfig>, String> {
    if !path.exists() {
        return Ok(vec![]);
    }

    let mut configs = read_connections(path)?;
    let mut needs_rewrite = false;
    for config in &mut configs {
        if config.password.is_empty() {
            if let Some(secret) = store.get_secret(&config.id, MAIN_PASSWORD_KEY)? {
                config.password = secret;
            }
        } else {
            store.set_secret(&config.id, MAIN_PASSWORD_KEY, &config.password)?;
            needs_rewrite = true;
        }

        hydrate_transport_layer_secrets(store, config, &mut needs_rewrite)?;

        if config.redis_sentinel_password.is_empty() {
            if let Some(secret) = store.get_secret(&config.id, REDIS_SENTINEL_PASSWORD_KEY)? {
                config.redis_sentinel_password = secret;
            }
        } else {
            store.set_secret(&config.id, REDIS_SENTINEL_PASSWORD_KEY, &config.redis_sentinel_password)?;
            needs_rewrite = true;
        }

        match config.connection_string.as_deref().filter(|secret| !secret.is_empty()) {
            Some(secret) => {
                store.set_secret(&config.id, CONNECTION_STRING_KEY, secret)?;
                needs_rewrite = true;
            }
            None => {
                if let Some(secret) = store.get_secret(&config.id, CONNECTION_STRING_KEY)? {
                    config.connection_string = Some(secret);
                }
            }
        }

        match config.init_script.as_deref().filter(|secret| !secret.is_empty()) {
            Some(secret) => {
                store.set_secret(&config.id, INIT_SCRIPT_KEY, secret)?;
                needs_rewrite = true;
            }
            None => {
                if let Some(secret) = store.get_secret(&config.id, INIT_SCRIPT_KEY)? {
                    config.init_script = Some(secret);
                }
            }
        }
        hydrate_mq_auth_secrets(store, config, &mut needs_rewrite)?;
        hydrate_mq_token_signing_secret(store, config, &mut needs_rewrite)?;
        hydrate_mqtt_auth_secrets(store, config, &mut needs_rewrite)?;
        hydrate_cassandra_tls_secrets(store, config, &mut needs_rewrite)?;
        let plugin_secret_keys = config.connection_secrets.keys().cloned().collect::<Vec<_>>();
        for key in plugin_secret_keys {
            let storage_key = plugin_connection_secret_key(&key)?;
            let current = config.connection_secrets.get(&key).cloned().unwrap_or_default();
            if current.is_empty() {
                if let Some(secret) = store.get_secret(&config.id, &storage_key)? {
                    config.connection_secrets.insert(key, secret);
                }
            } else {
                store.set_secret(&config.id, &storage_key, &current)?;
                needs_rewrite = true;
            }
        }
    }

    if needs_rewrite {
        write_sanitized_connections(path, &configs)?;
    }

    Ok(configs)
}

fn persist_transport_layer_secrets(
    store: &dyn ConnectionSecretStore,
    connection_id: &str,
    index: usize,
    layer: &TransportLayerConfig,
) -> Result<(), String> {
    match layer {
        TransportLayerConfig::Ssh(ssh) => {
            persist_secret(store, connection_id, &transport_layer_ssh_password_key(index, layer), &ssh.password)?;
            persist_secret(
                store,
                connection_id,
                &transport_layer_ssh_key_passphrase_key(index, layer),
                &ssh.key_passphrase,
            )?;
        }
        TransportLayerConfig::Proxy(proxy) => {
            persist_secret(store, connection_id, &transport_layer_proxy_password_key(index, layer), &proxy.password)?;
        }
        TransportLayerConfig::HttpTunnel(http) => {
            persist_secret(store, connection_id, &transport_layer_http_tunnel_token_key(index, layer), &http.token)?;
        }
    }
    Ok(())
}

fn hydrate_transport_layer_secrets(
    store: &dyn ConnectionSecretStore,
    config: &mut ConnectionConfig,
    needs_rewrite: &mut bool,
) -> Result<(), String> {
    for index in 0..config.transport_layers.len() {
        let layer_for_key = config.transport_layers[index].clone();
        match &mut config.transport_layers[index] {
            TransportLayerConfig::Ssh(ssh) => {
                let password_key = transport_layer_ssh_password_key(index, &layer_for_key);
                if ssh.password.is_empty() {
                    if let Some(secret) = store.get_secret(&config.id, &password_key)?.or(legacy_ssh_password_secret(
                        store,
                        &config.id,
                        index,
                        &layer_for_key,
                    )?) {
                        ssh.password = secret;
                    }
                } else {
                    store.set_secret(&config.id, &password_key, &ssh.password)?;
                    *needs_rewrite = true;
                }

                let passphrase_key = transport_layer_ssh_key_passphrase_key(index, &layer_for_key);
                if ssh.key_passphrase.is_empty() {
                    if let Some(secret) = store
                        .get_secret(&config.id, &passphrase_key)?
                        .or(legacy_ssh_key_passphrase_secret(store, &config.id, index, &layer_for_key)?)
                    {
                        ssh.key_passphrase = secret;
                    }
                } else {
                    store.set_secret(&config.id, &passphrase_key, &ssh.key_passphrase)?;
                    *needs_rewrite = true;
                }
            }
            TransportLayerConfig::Proxy(proxy) => {
                let password_key = transport_layer_proxy_password_key(index, &layer_for_key);
                if proxy.password.is_empty() {
                    if let Some(secret) = store.get_secret(&config.id, &password_key)?.or(legacy_proxy_password_secret(
                        store,
                        &config.id,
                        &layer_for_key,
                    )?) {
                        proxy.password = secret;
                    }
                } else {
                    store.set_secret(&config.id, &password_key, &proxy.password)?;
                    *needs_rewrite = true;
                }
            }
            TransportLayerConfig::HttpTunnel(http) => {
                let token_key = transport_layer_http_tunnel_token_key(index, &layer_for_key);
                if http.token.is_empty() {
                    if let Some(secret) = store.get_secret(&config.id, &token_key)? {
                        http.token = secret;
                    }
                } else {
                    store.set_secret(&config.id, &token_key, &http.token)?;
                    *needs_rewrite = true;
                }
            }
        }
    }
    Ok(())
}

fn legacy_ssh_password_secret(
    store: &dyn ConnectionSecretStore,
    connection_id: &str,
    index: usize,
    layer: &TransportLayerConfig,
) -> Result<Option<String>, String> {
    if let TransportLayerConfig::Ssh(ssh) = layer {
        if ssh.id == "legacy" {
            if let Some(secret) = store.get_secret(connection_id, SSH_PASSWORD_KEY)? {
                return Ok(Some(secret));
            }
        }
        store.get_secret(connection_id, &ssh_tunnel_password_key(index, ssh))
    } else {
        Ok(None)
    }
}

fn legacy_ssh_key_passphrase_secret(
    store: &dyn ConnectionSecretStore,
    connection_id: &str,
    index: usize,
    layer: &TransportLayerConfig,
) -> Result<Option<String>, String> {
    if let TransportLayerConfig::Ssh(ssh) = layer {
        if ssh.id == "legacy" {
            if let Some(secret) = store.get_secret(connection_id, SSH_KEY_PASSPHRASE_KEY)? {
                return Ok(Some(secret));
            }
        }
        store.get_secret(connection_id, &ssh_tunnel_key_passphrase_key(index, ssh))
    } else {
        Ok(None)
    }
}

fn legacy_proxy_password_secret(
    store: &dyn ConnectionSecretStore,
    connection_id: &str,
    layer: &TransportLayerConfig,
) -> Result<Option<String>, String> {
    if matches!(layer, TransportLayerConfig::Proxy(proxy) if proxy.id == "legacy-proxy") {
        store.get_secret(connection_id, PROXY_PASSWORD_KEY)
    } else {
        Ok(None)
    }
}

fn delete_removed_connection_secrets(
    path: &Path,
    configs: &[ConnectionConfig],
    store: &dyn ConnectionSecretStore,
) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }

    let previous = match read_connections(path) {
        Ok(configs) => configs,
        Err(_) => return Ok(()),
    };
    let current_ids: HashSet<&str> = configs.iter().map(|config| config.id.as_str()).collect();
    for config in previous {
        if current_ids.contains(config.id.as_str()) {
            continue;
        }
        store.delete_secret(&config.id, MAIN_PASSWORD_KEY)?;
        store.delete_secret(&config.id, SSH_PASSWORD_KEY)?;
        store.delete_secret(&config.id, SSH_KEY_PASSPHRASE_KEY)?;
        delete_secret_prefix(store, &config.id, SSH_TUNNEL_SECRET_PREFIX)?;
        delete_secret_prefix(store, &config.id, TRANSPORT_LAYER_SECRET_PREFIX)?;
        store.delete_secret(&config.id, CONNECTION_STRING_KEY)?;
        store.delete_secret(&config.id, INIT_SCRIPT_KEY)?;
        delete_secret_prefix(store, &config.id, MQ_AUTH_SECRET_PREFIX)?;
        delete_secret_prefix(store, &config.id, MQ_TOKEN_SIGNING_SECRET_PREFIX)?;
        delete_secret_prefix(store, &config.id, MQTT_AUTH_SECRET_PREFIX)?;
        delete_secret_prefix(store, &config.id, CASSANDRA_TLS_SECRET_PREFIX)?;
    }
    Ok(())
}

fn persist_secret(
    store: &dyn ConnectionSecretStore,
    connection_id: &str,
    key: &str,
    secret: &str,
) -> Result<(), String> {
    if secret.is_empty() {
        store.delete_secret(connection_id, key)
    } else {
        store.set_secret(connection_id, key, secret)
    }
}

fn persist_optional_secret(
    store: &dyn ConnectionSecretStore,
    connection_id: &str,
    key: &str,
    secret: Option<&str>,
) -> Result<(), String> {
    match secret.filter(|secret| !secret.is_empty()) {
        Some(secret) => store.set_secret(connection_id, key, secret),
        None => store.delete_secret(connection_id, key),
    }
}

fn delete_secret_prefix(
    store: &dyn ConnectionSecretStore,
    connection_id: &str,
    key_prefix: &str,
) -> Result<(), String> {
    store.delete_secret_prefix(connection_id, key_prefix)
}

fn persist_mq_auth_secrets(store: &dyn ConnectionSecretStore, config: &ConnectionConfig) -> Result<(), String> {
    if config.db_type != DatabaseType::MessageQueue {
        delete_secret_prefix(store, &config.id, MQ_AUTH_SECRET_PREFIX)?;
        return Ok(());
    }

    let Some(auth) = mq_auth_object(config.external_config.as_ref()) else {
        delete_secret_prefix(store, &config.id, MQ_AUTH_SECRET_PREFIX)?;
        return Ok(());
    };

    match mq_auth_kind(auth).as_deref() {
        Some("none") => delete_secret_prefix(store, &config.id, MQ_AUTH_SECRET_PREFIX)?,
        Some("token") => replace_mq_auth_secret(store, &config.id, MQ_AUTH_TOKEN_KEY, auth, "token")?,
        Some("basic") => replace_mq_auth_secret(store, &config.id, MQ_AUTH_PASSWORD_KEY, auth, "password")?,
        Some("apiKey") | Some("api_key") | Some("apikey") => {
            replace_mq_auth_secret(store, &config.id, MQ_AUTH_API_KEY_VALUE_KEY, auth, "value")?
        }
        Some("oauth2") => replace_mq_auth_secret(store, &config.id, MQ_AUTH_CLIENT_SECRET_KEY, auth, "clientSecret")?,
        _ => delete_secret_prefix(store, &config.id, MQ_AUTH_SECRET_PREFIX)?,
    }

    Ok(())
}

fn replace_mq_auth_secret(
    store: &dyn ConnectionSecretStore,
    connection_id: &str,
    key: &str,
    auth: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<(), String> {
    let current = auth.get(field).and_then(serde_json::Value::as_str).filter(|secret| !secret.is_empty());
    let existing = if current.is_none() { store.get_secret(connection_id, key)? } else { None };
    delete_secret_prefix(store, connection_id, MQ_AUTH_SECRET_PREFIX)?;
    match current {
        Some(secret) => store.set_secret(connection_id, key, secret),
        None => match existing {
            Some(secret) => store.set_secret(connection_id, key, &secret),
            None => Ok(()),
        },
    }
}

fn persist_mq_token_signing_secret(store: &dyn ConnectionSecretStore, config: &ConnectionConfig) -> Result<(), String> {
    if config.db_type != DatabaseType::MessageQueue {
        delete_secret_prefix(store, &config.id, MQ_TOKEN_SIGNING_SECRET_PREFIX)?;
        return Ok(());
    }

    let Some(signing) = mq_token_signing_object(config.external_config.as_ref()) else {
        delete_secret_prefix(store, &config.id, MQ_TOKEN_SIGNING_SECRET_PREFIX)?;
        return Ok(());
    };

    persist_json_secret_if_present(store, &config.id, MQ_TOKEN_SIGNING_KEY, signing, "key")
}

fn hydrate_mq_auth_secrets(
    store: &dyn ConnectionSecretStore,
    config: &mut ConnectionConfig,
    needs_rewrite: &mut bool,
) -> Result<(), String> {
    if config.db_type != DatabaseType::MessageQueue {
        return Ok(());
    }

    let Some(auth) = mq_auth_object_mut(config.external_config.as_mut()) else {
        return Ok(());
    };

    match mq_auth_kind(auth).as_deref() {
        Some("token") => hydrate_json_secret(store, &config.id, MQ_AUTH_TOKEN_KEY, auth, "token", needs_rewrite)?,
        Some("basic") => hydrate_json_secret(store, &config.id, MQ_AUTH_PASSWORD_KEY, auth, "password", needs_rewrite)?,
        Some("apiKey") | Some("api_key") | Some("apikey") => {
            hydrate_json_secret(store, &config.id, MQ_AUTH_API_KEY_VALUE_KEY, auth, "value", needs_rewrite)?
        }
        Some("oauth2") => {
            hydrate_json_secret(store, &config.id, MQ_AUTH_CLIENT_SECRET_KEY, auth, "clientSecret", needs_rewrite)?
        }
        _ => {}
    }

    Ok(())
}

fn hydrate_mq_token_signing_secret(
    store: &dyn ConnectionSecretStore,
    config: &mut ConnectionConfig,
    needs_rewrite: &mut bool,
) -> Result<(), String> {
    if config.db_type != DatabaseType::MessageQueue {
        return Ok(());
    }

    let Some(signing) = mq_token_signing_object_mut(config.external_config.as_mut()) else {
        return Ok(());
    };

    hydrate_json_secret(store, &config.id, MQ_TOKEN_SIGNING_KEY, signing, "key", needs_rewrite)
}

fn persist_json_secret_if_present(
    store: &dyn ConnectionSecretStore,
    connection_id: &str,
    key: &str,
    auth: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<(), String> {
    match auth.get(field).and_then(serde_json::Value::as_str).filter(|secret| !secret.is_empty()) {
        Some(secret) => store.set_secret(connection_id, key, secret),
        None => Ok(()),
    }
}

fn hydrate_json_secret(
    store: &dyn ConnectionSecretStore,
    connection_id: &str,
    key: &str,
    auth: &mut serde_json::Map<String, serde_json::Value>,
    field: &str,
    needs_rewrite: &mut bool,
) -> Result<(), String> {
    match auth.get(field).and_then(serde_json::Value::as_str).filter(|secret| !secret.is_empty()) {
        Some(secret) => {
            store.set_secret(connection_id, key, secret)?;
            *needs_rewrite = true;
        }
        None => {
            if let Some(secret) = store.get_secret(connection_id, key)? {
                auth.insert(field.to_string(), serde_json::Value::String(secret));
            }
        }
    }
    Ok(())
}

fn scrub_mq_auth_secrets(config: &mut ConnectionConfig) {
    let Some(auth) = mq_auth_object_mut(config.external_config.as_mut()) else {
        return;
    };
    match mq_auth_kind(auth).as_deref() {
        Some("token") => scrub_json_secret(auth, "token"),
        Some("basic") => scrub_json_secret(auth, "password"),
        Some("apiKey") | Some("api_key") | Some("apikey") => scrub_json_secret(auth, "value"),
        Some("oauth2") => scrub_json_secret(auth, "clientSecret"),
        _ => {}
    }
}

fn scrub_mq_token_signing_secret(config: &mut ConnectionConfig) {
    let Some(signing) = mq_token_signing_object_mut(config.external_config.as_mut()) else {
        return;
    };
    scrub_json_secret(signing, "key");
}

fn persist_cassandra_tls_secrets(store: &dyn ConnectionSecretStore, config: &ConnectionConfig) -> Result<(), String> {
    if config.db_type != DatabaseType::Cassandra {
        return delete_secret_prefix(store, &config.id, CASSANDRA_TLS_SECRET_PREFIX);
    }
    let Some(tls) = cassandra_tls_object(config.external_config.as_ref()) else {
        return delete_secret_prefix(store, &config.id, CASSANDRA_TLS_SECRET_PREFIX);
    };
    persist_secret(
        store,
        &config.id,
        CASSANDRA_TRUSTSTORE_PASSWORD_KEY,
        tls.get("truststore_password").and_then(serde_json::Value::as_str).unwrap_or(""),
    )?;
    persist_secret(
        store,
        &config.id,
        CASSANDRA_KEYSTORE_PASSWORD_KEY,
        tls.get("keystore_password").and_then(serde_json::Value::as_str).unwrap_or(""),
    )
}

fn hydrate_cassandra_tls_secrets(
    store: &dyn ConnectionSecretStore,
    config: &mut ConnectionConfig,
    needs_rewrite: &mut bool,
) -> Result<(), String> {
    if config.db_type != DatabaseType::Cassandra {
        return Ok(());
    }
    let connection_id = config.id.clone();
    let Some(tls) = cassandra_tls_object_mut(config.external_config.as_mut()) else {
        return Ok(());
    };
    hydrate_json_secret(
        store,
        &connection_id,
        CASSANDRA_TRUSTSTORE_PASSWORD_KEY,
        tls,
        "truststore_password",
        needs_rewrite,
    )?;
    hydrate_json_secret(store, &connection_id, CASSANDRA_KEYSTORE_PASSWORD_KEY, tls, "keystore_password", needs_rewrite)
}

fn scrub_cassandra_tls_secrets(config: &mut ConnectionConfig) {
    if config.db_type != DatabaseType::Cassandra {
        return;
    }
    let Some(tls) = cassandra_tls_object_mut(config.external_config.as_mut()) else {
        return;
    };
    scrub_json_secret(tls, "truststore_password");
    scrub_json_secret(tls, "keystore_password");
}

fn cassandra_tls_object(value: Option<&serde_json::Value>) -> Option<&serde_json::Map<String, serde_json::Value>> {
    value?.get("tls")?.as_object()
}

fn cassandra_tls_object_mut(
    value: Option<&mut serde_json::Value>,
) -> Option<&mut serde_json::Map<String, serde_json::Value>> {
    value?.get_mut("tls")?.as_object_mut()
}

// ── MQTT 密钥持久化 ──────────────────────────────────────────────

fn persist_mqtt_auth_secrets(store: &dyn ConnectionSecretStore, config: &ConnectionConfig) -> Result<(), String> {
    if config.db_type != DatabaseType::Mqtt {
        delete_secret_prefix(store, &config.id, MQTT_AUTH_SECRET_PREFIX)?;
        return Ok(());
    }

    let Some(auth) = mqtt_auth_object(config.external_config.as_ref()) else {
        delete_secret_prefix(store, &config.id, MQTT_AUTH_SECRET_PREFIX)?;
        return Ok(());
    };

    match mqtt_auth_kind(auth).as_deref() {
        Some("password") => replace_mqtt_auth_secret(store, &config.id, MQTT_AUTH_PASSWORD_KEY, auth, "password")?,
        _ => delete_secret_prefix(store, &config.id, MQTT_AUTH_SECRET_PREFIX)?,
    }

    Ok(())
}

fn hydrate_mqtt_auth_secrets(
    store: &dyn ConnectionSecretStore,
    config: &mut ConnectionConfig,
    needs_rewrite: &mut bool,
) -> Result<(), String> {
    if config.db_type != DatabaseType::Mqtt {
        return Ok(());
    }

    let Some(auth) = mqtt_auth_object_mut(config.external_config.as_mut()) else {
        return Ok(());
    };

    if let Some("password") = mqtt_auth_kind(auth).as_deref() {
        hydrate_json_secret(store, &config.id, MQTT_AUTH_PASSWORD_KEY, auth, "password", needs_rewrite)?
    }

    Ok(())
}

fn scrub_mqtt_auth_secrets(config: &mut ConnectionConfig) {
    let Some(auth) = mqtt_auth_object_mut(config.external_config.as_mut()) else {
        return;
    };
    if let Some("password") = mqtt_auth_kind(auth).as_deref() {
        scrub_json_secret(auth, "password")
    }
}

fn replace_mqtt_auth_secret(
    store: &dyn ConnectionSecretStore,
    connection_id: &str,
    key: &str,
    auth: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<(), String> {
    let current = auth.get(field).and_then(serde_json::Value::as_str).filter(|secret| !secret.is_empty());
    let existing = if current.is_none() { store.get_secret(connection_id, key)? } else { None };
    delete_secret_prefix(store, connection_id, MQTT_AUTH_SECRET_PREFIX)?;
    match current {
        Some(secret) => store.set_secret(connection_id, key, secret),
        None => match existing {
            Some(secret) => store.set_secret(connection_id, key, &secret),
            None => Ok(()),
        },
    }
}

fn mqtt_auth_kind(auth: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    auth.get("kind").and_then(serde_json::Value::as_str).map(ToString::to_string)
}

fn mqtt_auth_object(value: Option<&serde_json::Value>) -> Option<&serde_json::Map<String, serde_json::Value>> {
    value?.get("auth")?.as_object()
}

fn mqtt_auth_object_mut(
    value: Option<&mut serde_json::Value>,
) -> Option<&mut serde_json::Map<String, serde_json::Value>> {
    value?.get_mut("auth")?.as_object_mut()
}

fn scrub_json_secret(auth: &mut serde_json::Map<String, serde_json::Value>, field: &str) {
    if auth.contains_key(field) {
        auth.insert(field.to_string(), serde_json::Value::String(String::new()));
    }
}

fn mq_auth_kind(auth: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    auth.get("kind").and_then(serde_json::Value::as_str).map(ToString::to_string)
}

fn mq_auth_object(value: Option<&serde_json::Value>) -> Option<&serde_json::Map<String, serde_json::Value>> {
    value?.get("auth")?.as_object()
}

fn mq_auth_object_mut(
    value: Option<&mut serde_json::Value>,
) -> Option<&mut serde_json::Map<String, serde_json::Value>> {
    value?.get_mut("auth")?.as_object_mut()
}

fn mq_token_signing_object(value: Option<&serde_json::Value>) -> Option<&serde_json::Map<String, serde_json::Value>> {
    value?.get("tokenSigning")?.as_object()
}

fn mq_token_signing_object_mut(
    value: Option<&mut serde_json::Value>,
) -> Option<&mut serde_json::Map<String, serde_json::Value>> {
    value?.get_mut("tokenSigning")?.as_object_mut()
}

fn ssh_tunnel_secret_segment(index: usize, hop: &crate::models::connection::SshTunnelConfig) -> String {
    if hop.id.trim().is_empty() {
        index.to_string()
    } else {
        hop.id.clone()
    }
}

fn ssh_tunnel_password_key(index: usize, hop: &crate::models::connection::SshTunnelConfig) -> String {
    format!("{}{}.password", SSH_TUNNEL_SECRET_PREFIX, ssh_tunnel_secret_segment(index, hop))
}

fn ssh_tunnel_key_passphrase_key(index: usize, hop: &crate::models::connection::SshTunnelConfig) -> String {
    format!("{}{}.key_passphrase", SSH_TUNNEL_SECRET_PREFIX, ssh_tunnel_secret_segment(index, hop))
}

fn transport_layer_secret_segment(index: usize, layer: &TransportLayerConfig) -> String {
    let id = layer.id().trim();
    if id.is_empty() {
        index.to_string()
    } else {
        id.to_string()
    }
}

fn transport_layer_ssh_password_key(index: usize, layer: &TransportLayerConfig) -> String {
    format!("{}{}.ssh_password", TRANSPORT_LAYER_SECRET_PREFIX, transport_layer_secret_segment(index, layer))
}

fn transport_layer_ssh_key_passphrase_key(index: usize, layer: &TransportLayerConfig) -> String {
    format!("{}{}.ssh_key_passphrase", TRANSPORT_LAYER_SECRET_PREFIX, transport_layer_secret_segment(index, layer))
}

fn transport_layer_proxy_password_key(index: usize, layer: &TransportLayerConfig) -> String {
    format!("{}{}.proxy_password", TRANSPORT_LAYER_SECRET_PREFIX, transport_layer_secret_segment(index, layer))
}

fn transport_layer_http_tunnel_token_key(index: usize, layer: &TransportLayerConfig) -> String {
    format!("{}{}.http_tunnel_token", TRANSPORT_LAYER_SECRET_PREFIX, transport_layer_secret_segment(index, layer))
}

fn read_connections(path: &Path) -> Result<Vec<ConnectionConfig>, String> {
    let json = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&json).map_err(|e| e.to_string())
}

fn write_sanitized_connections(path: &Path, configs: &[ConnectionConfig]) -> Result<(), String> {
    let sanitized = sanitize_connections(configs);
    let json = serde_json::to_string_pretty(&sanitized).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

fn sanitize_connections(configs: &[ConnectionConfig]) -> Vec<ConnectionConfig> {
    configs
        .iter()
        .cloned()
        .map(|mut config| {
            config.password.clear();
            for layer in &mut config.transport_layers {
                match layer {
                    TransportLayerConfig::Ssh(ssh) => {
                        ssh.password.clear();
                        ssh.key_passphrase.clear();
                    }
                    TransportLayerConfig::Proxy(proxy) => {
                        proxy.password.clear();
                    }
                    TransportLayerConfig::HttpTunnel(http) => {
                        http.token.clear();
                    }
                }
            }
            config.redis_sentinel_password.clear();
            config.connection_string = None;
            config.init_script = None;
            scrub_mq_auth_secrets(&mut config);
            scrub_mq_token_signing_secret(&mut config);
            scrub_mqtt_auth_secrets(&mut config);
            scrub_cassandra_tls_secrets(&mut config);
            scrub_plugin_connection_secrets(&mut config);
            config
        })
        .collect()
}

fn scrub_plugin_connection_secrets(config: &mut ConnectionConfig) {
    for secret in config.connection_secrets.values_mut() {
        secret.clear();
    }
}

// ---------------------------------------------------------------------------
// Client-facing secret redaction
//
// Saved connection secrets never leave the backend. Configurations sent to a
// UI are redacted: every secret slot is blanked and its client path is listed
// in `saved_secrets`. When a UI sends a configuration back (save, test,
// connect, ...), blank slots are filled from the stored configuration of the
// same connection id unless the UI explicitly listed the path in
// `cleared_secrets`.
// ---------------------------------------------------------------------------

/// Field added to redacted configurations sent to a client.
pub const CLIENT_SAVED_SECRETS_FIELD: &str = "saved_secrets";
/// Optional field of a client configuration: secret paths to remove.
pub const CLIENT_CLEARED_SECRETS_FIELD: &str = "cleared_secrets";
/// Optional field of a client configuration: saved connection whose stored
/// secrets fill blank fields when this id has no stored record (duplicates).
pub const CLIENT_SECRETS_FROM_FIELD: &str = "secrets_from_connection_id";

/// A connection configuration received from a client together with its
/// secret directives. Deserializes from the plain configuration object; the
/// directive fields are removed before the configuration is parsed.
#[derive(Debug, Clone)]
pub struct ClientConnectionInput {
    pub config: ConnectionConfig,
    pub cleared_secrets: Vec<String>,
    pub secrets_from_connection_id: Option<String>,
}

impl From<ConnectionConfig> for ClientConnectionInput {
    fn from(config: ConnectionConfig) -> Self {
        Self { config, cleared_secrets: Vec::new(), secrets_from_connection_id: None }
    }
}

impl ClientConnectionInput {
    pub fn from_value(mut value: serde_json::Value) -> Result<Self, String> {
        let Some(object) = value.as_object_mut() else {
            return Err("Connection configuration must be a JSON object".to_string());
        };
        object.remove(CLIENT_SAVED_SECRETS_FIELD);
        let cleared_secrets = match object.remove(CLIENT_CLEARED_SECRETS_FIELD) {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(value) => serde_json::from_value::<Vec<String>>(value)
                .map_err(|error| format!("Invalid {CLIENT_CLEARED_SECRETS_FIELD}: {error}"))?,
        };
        let secrets_from_connection_id = match object.remove(CLIENT_SECRETS_FROM_FIELD) {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(id)) => Some(id).filter(|id| !id.trim().is_empty()),
            Some(_) => return Err(format!("Invalid {CLIENT_SECRETS_FROM_FIELD}")),
        };
        let config = serde_json::from_value(value).map_err(|error| error.to_string())?;
        Ok(Self { config, cleared_secrets, secrets_from_connection_id })
    }
}

impl<'de> serde::Deserialize<'de> for ClientConnectionInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
        Self::from_value(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretSlotKind {
    /// A string field; blank means `""`.
    Text,
    /// An optional string field; blank means `null`.
    Optional,
}

#[derive(Debug, Clone)]
struct SecretSlot {
    path: String,
    /// JSON pointer of the object that holds the field (`""` = root).
    parent: String,
    field: String,
    kind: SecretSlotKind,
}

impl SecretSlot {
    fn text(path: impl Into<String>, parent: impl Into<String>, field: impl Into<String>) -> Self {
        Self { path: path.into(), parent: parent.into(), field: field.into(), kind: SecretSlotKind::Text }
    }

    fn optional(field: &str) -> Self {
        Self {
            path: field.to_string(),
            parent: String::new(),
            field: field.to_string(),
            kind: SecretSlotKind::Optional,
        }
    }
}

fn json_pointer_segment(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

/// Client path segment of a transport layer: its id, or `#<index>` for
/// legacy layers without an id.
pub fn transport_layer_client_segment(index: usize, layer: &TransportLayerConfig) -> String {
    let id = layer.id().trim();
    if id.is_empty() {
        format!("#{index}")
    } else {
        id.to_string()
    }
}

fn external_object<'a>(
    config: &'a ConnectionConfig,
    key: &str,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    config.external_config.as_ref()?.get(key)?.as_object()
}

fn external_kind(object: &serde_json::Map<String, serde_json::Value>) -> Option<&str> {
    object.get("kind").and_then(serde_json::Value::as_str)
}

fn external_slot(object_key: &str, field: &str) -> SecretSlot {
    SecretSlot::text(
        format!("external_config.{object_key}.{field}"),
        format!("/external_config/{}", json_pointer_segment(object_key)),
        field,
    )
}

/// Secret slots of the structured (non-plugin) connection fields. Plugin
/// secrets (`connection_secrets`) are handled separately because the map
/// keys are provider-defined.
fn connection_secret_slots(config: &ConnectionConfig) -> Vec<SecretSlot> {
    let mut slots = vec![
        SecretSlot::text("password", "", "password"),
        SecretSlot::text("redis_sentinel_password", "", "redis_sentinel_password"),
        SecretSlot::optional("connection_string"),
        SecretSlot::optional("init_script"),
    ];
    for (index, layer) in config.transport_layers.iter().enumerate() {
        let segment = transport_layer_client_segment(index, layer);
        let parent = format!("/transport_layers/{index}");
        let fields: &[&str] = match layer {
            TransportLayerConfig::Ssh(_) => &["password", "key_passphrase"],
            TransportLayerConfig::Proxy(_) => &["password"],
            TransportLayerConfig::HttpTunnel(_) => &["token"],
        };
        for field in fields {
            slots.push(SecretSlot::text(format!("transport_layers.{segment}.{field}"), parent.clone(), *field));
        }
    }
    match config.db_type {
        DatabaseType::MessageQueue => {
            if let Some(auth) = external_object(config, "auth") {
                let field = match external_kind(auth) {
                    Some("token") => Some("token"),
                    Some("basic") => Some("password"),
                    Some("apiKey" | "api_key" | "apikey") => Some("value"),
                    Some("oauth2") => Some("clientSecret"),
                    _ => None,
                };
                if let Some(field) = field {
                    slots.push(external_slot("auth", field));
                }
            }
            if external_object(config, "tokenSigning").is_some() {
                slots.push(external_slot("tokenSigning", "key"));
            }
        }
        DatabaseType::Mqtt => {
            if external_object(config, "auth").is_some_and(|auth| external_kind(auth) == Some("password")) {
                slots.push(external_slot("auth", "password"));
            }
        }
        DatabaseType::Nacos => {
            if external_object(config, "auth").is_some_and(|auth| external_kind(auth) == Some("usernamePassword")) {
                slots.push(external_slot("auth", "password"));
            }
            if external_object(config, "rnacosConsoleAuth")
                .is_some_and(|auth| external_kind(auth) == Some("usernamePassword"))
            {
                slots.push(external_slot("rnacosConsoleAuth", "password"));
            }
        }
        DatabaseType::Cassandra if external_object(config, "tls").is_some() => {
            slots.push(external_slot("tls", "truststore_password"));
            slots.push(external_slot("tls", "keystore_password"));
        }
        _ => {}
    }
    slots
}

fn slot_value(value: &serde_json::Value, slot: &SecretSlot) -> String {
    let parent = if slot.parent.is_empty() { Some(value) } else { value.pointer(&slot.parent) };
    parent
        .and_then(|parent| parent.get(&slot.field))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn set_slot_value(value: &mut serde_json::Value, slot: &SecretSlot, secret: Option<&str>) {
    let parent = if slot.parent.is_empty() { Some(value) } else { value.pointer_mut(&slot.parent) };
    let Some(object) = parent.and_then(serde_json::Value::as_object_mut) else {
        return;
    };
    let replacement = match (secret, slot.kind) {
        (Some(secret), _) => serde_json::Value::String(secret.to_string()),
        (None, SecretSlotKind::Text) => {
            if !object.contains_key(&slot.field) {
                return;
            }
            serde_json::Value::String(String::new())
        }
        (None, SecretSlotKind::Optional) => serde_json::Value::Null,
    };
    object.insert(slot.field.clone(), replacement);
}

fn url_param_key_is_sensitive(key: &str) -> bool {
    let normalized = key.trim().trim_start_matches('?').to_ascii_lowercase().replace(['_', '-', '.'], "");
    normalized.contains("password")
        || normalized.contains("passwd")
        || normalized == "pwd"
        || normalized.contains("passphrase")
        || normalized.contains("secret")
        || normalized.contains("token")
        || normalized.contains("apikey")
        || normalized.contains("privatekey")
        || normalized.contains("credential")
}

/// Splits `a=1&b=2;c=3` into segments, keeping each trailing separator.
fn split_url_params(value: &str) -> Vec<(&str, Option<char>)> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (index, ch) in value.char_indices() {
        if ch == '&' || ch == ';' {
            parts.push((&value[start..index], Some(ch)));
            start = index + ch.len_utf8();
        }
    }
    parts.push((&value[start..], None));
    parts
}

fn rebuild_url_params(parts: impl Iterator<Item = (String, Option<char>)>) -> String {
    let mut output = String::new();
    for (segment, separator) in parts {
        output.push_str(&segment);
        if let Some(separator) = separator {
            output.push(separator);
        }
    }
    output
}

/// Blanks the values of credential-like URL parameters. Returns the redacted
/// string and whether anything was hidden.
pub fn redact_url_params(value: &str) -> (String, bool) {
    let mut hidden = false;
    let parts = split_url_params(value)
        .into_iter()
        .map(|(segment, separator)| match segment.split_once('=') {
            Some((key, secret)) if url_param_key_is_sensitive(key) && !secret.is_empty() => {
                hidden = true;
                (format!("{key}="), separator)
            }
            _ => (segment.to_string(), separator),
        })
        .collect::<Vec<_>>();
    (rebuild_url_params(parts.into_iter()), hidden)
}

/// Fills blank credential-like URL parameters from the stored parameters.
fn merge_url_params(incoming: &str, stored: &str) -> String {
    let stored_values = split_url_params(stored)
        .into_iter()
        .filter_map(|(segment, _)| segment.split_once('='))
        .filter(|(key, value)| url_param_key_is_sensitive(key) && !value.is_empty())
        .map(|(key, value)| (key.trim().to_string(), value.to_string()))
        .fold(HashMap::new(), |mut values, (key, value)| {
            values.entry(key).or_insert(value);
            values
        });
    let parts = split_url_params(incoming)
        .into_iter()
        .map(|(segment, separator)| match segment.split_once('=') {
            Some((key, "")) if url_param_key_is_sensitive(key) => match stored_values.get(key.trim()) {
                Some(secret) => (format!("{key}={secret}"), separator),
                None => (segment.to_string(), separator),
            },
            _ => (segment.to_string(), separator),
        })
        .collect::<Vec<_>>();
    rebuild_url_params(parts.into_iter())
}

/// Serializes a configuration for a client with every secret removed and a
/// `saved_secrets` list naming the secrets that exist.
pub fn redact_connection_for_client(config: &ConnectionConfig) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(config).map_err(|error| error.to_string())?;
    let mut saved = Vec::new();
    for slot in connection_secret_slots(config) {
        if !slot_value(&value, &slot).is_empty() {
            saved.push(slot.path.clone());
        }
        set_slot_value(&mut value, &slot, None);
    }
    if let Some(url_params) = config.url_params.as_deref() {
        let (redacted, hidden) = redact_url_params(url_params);
        if hidden {
            saved.push("url_params".to_string());
        }
        if let Some(object) = value.as_object_mut() {
            object.insert("url_params".to_string(), serde_json::Value::String(redacted));
        }
    }
    let mut plugin_keys = config.connection_secrets.iter().collect::<Vec<_>>();
    plugin_keys.sort_by(|left, right| left.0.cmp(right.0));
    for (key, secret) in plugin_keys {
        if !secret.is_empty() {
            saved.push(format!("connection_secrets.{key}"));
        }
    }
    let Some(object) = value.as_object_mut() else {
        return Err("Connection configuration must serialize to an object".to_string());
    };
    object.remove("connection_secrets");
    object.insert(
        CLIENT_SAVED_SECRETS_FIELD.to_string(),
        serde_json::Value::Array(saved.into_iter().map(serde_json::Value::String).collect()),
    );
    Ok(value)
}

pub fn redact_connections_for_client(configs: &[ConnectionConfig]) -> Result<Vec<serde_json::Value>, String> {
    configs.iter().map(redact_connection_for_client).collect()
}

/// Paths whose stored value must not be reused for this configuration, for
/// example the database password of a connection that does not save it.
fn secret_path_excluded(config: &ConnectionConfig, path: &str) -> bool {
    !config.save_password
        && (path == "password"
            || (config.db_type == DatabaseType::Nacos
                && matches!(path, "external_config.auth.password" | "external_config.rnacosConsoleAuth.password")))
}

/// Fills blank secret fields of a client configuration from the stored
/// configuration. Non-empty client values always win; `cleared` paths are
/// blanked and never filled.
pub fn merge_stored_connection_secrets(
    incoming: &ConnectionConfig,
    stored: Option<&ConnectionConfig>,
    cleared: &[String],
) -> Result<ConnectionConfig, String> {
    let cleared = cleared.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut value = serde_json::to_value(incoming).map_err(|error| error.to_string())?;
    let stored_value = stored.map(serde_json::to_value).transpose().map_err(|error| error.to_string())?;
    let stored_secrets = match (stored, stored_value.as_ref()) {
        (Some(stored), Some(stored_value)) => connection_secret_slots(stored)
            .into_iter()
            .filter_map(|slot| {
                let secret = slot_value(stored_value, &slot);
                (!secret.is_empty()).then_some((slot.path, secret))
            })
            .collect::<HashMap<_, _>>(),
        _ => HashMap::new(),
    };
    for slot in connection_secret_slots(incoming) {
        if cleared.contains(slot.path.as_str()) {
            set_slot_value(&mut value, &slot, None);
            continue;
        }
        if !slot_value(&value, &slot).is_empty() || secret_path_excluded(incoming, &slot.path) {
            continue;
        }
        if let Some(secret) = stored_secrets.get(&slot.path) {
            set_slot_value(&mut value, &slot, Some(secret));
        }
    }

    let object = value.as_object_mut().ok_or_else(|| "Connection configuration must be an object".to_string())?;
    let stored_url_params = stored.and_then(|stored| stored.url_params.as_deref()).filter(|value| !value.is_empty());
    if !cleared.contains("url_params") {
        match (incoming.url_params.as_deref(), stored_url_params) {
            (Some(incoming_params), Some(stored_params)) => {
                object.insert(
                    "url_params".to_string(),
                    serde_json::Value::String(merge_url_params(incoming_params, stored_params)),
                );
            }
            // An absent field keeps the stored parameters; an explicit empty
            // string is an edit that removes them.
            (None, Some(stored_params)) => {
                object.insert("url_params".to_string(), serde_json::Value::String(stored_params.to_string()));
            }
            _ => {}
        }
    }

    let mut plugin_secrets = incoming.connection_secrets.clone();
    plugin_secrets
        .retain(|key, secret| !(secret.is_empty() && cleared.contains(format!("connection_secrets.{key}").as_str())));
    if let Some(stored) = stored.filter(|stored| {
        stored.plugin_id == incoming.plugin_id
            && stored.plugin_connection_provider == incoming.plugin_connection_provider
    }) {
        for (key, secret) in &stored.connection_secrets {
            if secret.is_empty() || cleared.contains(format!("connection_secrets.{key}").as_str()) {
                continue;
            }
            let current = plugin_secrets.entry(key.clone()).or_default();
            if current.is_empty() {
                *current = secret.clone();
            }
        }
    }
    plugin_secrets.retain(|_, secret| !secret.is_empty());
    object.insert(
        "connection_secrets".to_string(),
        serde_json::to_value(&plugin_secrets).map_err(|error| error.to_string())?,
    );

    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// Storage keys that must be deleted for an explicitly cleared client path
/// whose persistence would otherwise keep the previously stored value.
pub fn storage_keys_for_cleared_secret(config: &ConnectionConfig, path: &str) -> Vec<&'static str> {
    match (config.db_type, path) {
        (_, "password") => vec![MAIN_PASSWORD_KEY],
        (_, "redis_sentinel_password") => vec![REDIS_SENTINEL_PASSWORD_KEY],
        (_, "connection_string") => vec![CONNECTION_STRING_KEY],
        (_, "init_script") => vec![INIT_SCRIPT_KEY],
        (DatabaseType::MessageQueue, "external_config.auth.token") => vec![MQ_AUTH_TOKEN_KEY],
        (DatabaseType::MessageQueue, "external_config.auth.password") => vec![MQ_AUTH_PASSWORD_KEY],
        (DatabaseType::MessageQueue, "external_config.auth.value") => vec![MQ_AUTH_API_KEY_VALUE_KEY],
        (DatabaseType::MessageQueue, "external_config.auth.clientSecret") => vec![MQ_AUTH_CLIENT_SECRET_KEY],
        (DatabaseType::MessageQueue, "external_config.tokenSigning.key") => vec![MQ_TOKEN_SIGNING_KEY],
        (DatabaseType::Mqtt, "external_config.auth.password") => vec![MQTT_AUTH_PASSWORD_KEY],
        (DatabaseType::Nacos, "external_config.auth.password") => vec![NACOS_AUTH_PASSWORD_KEY],
        (DatabaseType::Nacos, "external_config.rnacosConsoleAuth.password") => {
            vec![NACOS_RNACOS_CONSOLE_PASSWORD_KEY]
        }
        (DatabaseType::Cassandra, "external_config.tls.truststore_password") => {
            vec![CASSANDRA_TRUSTSTORE_PASSWORD_KEY]
        }
        (DatabaseType::Cassandra, "external_config.tls.keystore_password") => vec![CASSANDRA_KEYSTORE_PASSWORD_KEY],
        _ => Vec::new(),
    }
}

pub fn secret_account(connection_id: &str, key: &str) -> String {
    format!("connection:{connection_id}:{key}")
}

#[cfg(test)]
mod tests {
    use super::{
        load_connections_from_file, save_connections_to_file, ConnectionSecretStore, CASSANDRA_KEYSTORE_PASSWORD_KEY,
        CASSANDRA_TRUSTSTORE_PASSWORD_KEY, CONNECTION_STRING_KEY, INIT_SCRIPT_KEY, MAIN_PASSWORD_KEY,
        MQTT_AUTH_PASSWORD_KEY, MQ_AUTH_PASSWORD_KEY, MQ_AUTH_TOKEN_KEY, MQ_TOKEN_SIGNING_KEY,
        PLUGIN_CONNECTION_SECRET_PREFIX, REDIS_SENTINEL_PASSWORD_KEY, SSH_PASSWORD_KEY,
    };
    use crate::models::connection::{
        ConnectionConfig, DatabaseType, HttpTunnelConfig, SshTunnelConfig, TransportLayerConfig,
    };
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::Path;

    #[derive(Default)]
    struct MemorySecretStore {
        values: RefCell<HashMap<String, String>>,
        deleted: RefCell<Vec<String>>,
    }

    impl MemorySecretStore {
        fn set_existing(&self, connection_id: &str, key: &str, value: &str) {
            self.values.borrow_mut().insert(secret_key(connection_id, key), value.to_string());
        }

        fn get_existing(&self, connection_id: &str, key: &str) -> Option<String> {
            self.values.borrow().get(&secret_key(connection_id, key)).cloned()
        }

        fn was_deleted(&self, connection_id: &str, key: &str) -> bool {
            self.deleted.borrow().contains(&secret_key(connection_id, key))
        }
    }

    impl ConnectionSecretStore for MemorySecretStore {
        fn set_secret(&self, connection_id: &str, key: &str, secret: &str) -> Result<(), String> {
            self.values.borrow_mut().insert(secret_key(connection_id, key), secret.to_string());
            Ok(())
        }

        fn get_secret(&self, connection_id: &str, key: &str) -> Result<Option<String>, String> {
            Ok(self.values.borrow().get(&secret_key(connection_id, key)).cloned())
        }

        fn delete_secret(&self, connection_id: &str, key: &str) -> Result<(), String> {
            self.values.borrow_mut().remove(&secret_key(connection_id, key));
            self.deleted.borrow_mut().push(secret_key(connection_id, key));
            Ok(())
        }

        fn delete_secret_prefix(&self, connection_id: &str, key_prefix: &str) -> Result<(), String> {
            let prefix = secret_key(connection_id, key_prefix);
            self.values.borrow_mut().retain(|key, _| !key.starts_with(&prefix));
            self.deleted.borrow_mut().push(prefix);
            Ok(())
        }
    }

    fn secret_key(connection_id: &str, key: &str) -> String {
        format!("{connection_id}:{key}")
    }

    fn temp_connections_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dbx-connection-secrets-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("connections.json")
    }

    fn connection(id: &str, password: &str, _ssh_password: &str) -> ConnectionConfig {
        ConnectionConfig {
            docs_notes_path: None,
            id: id.to_string(),
            name: format!("{id} connection"),
            note: String::new(),
            db_type: DatabaseType::Postgres,
            driver_profile: None,
            driver_label: None,
            url_params: None,
            agent_java_options: Vec::new(),
            host: "localhost".to_string(),
            port: 5432,
            username: "postgres".to_string(),
            password: password.to_string(),
            database: Some("postgres".to_string()),
            default_schema: None,
            visible_databases: None,
            visible_database_patterns: None,
            visible_schemas: None,
            show_system_schemas: false,
            attached_databases: Vec::new(),
            init_script: None,
            color: None,
            transport_layers: Vec::new(),
            connect_timeout_secs: crate::models::connection::default_connect_timeout_secs(),
            query_timeout_secs: crate::models::connection::default_query_timeout_secs(),
            idle_timeout_secs: crate::models::connection::default_idle_timeout_secs(),
            keepalive_interval_secs: crate::models::connection::default_keepalive_interval_secs(),
            ssl: false,
            ca_cert_path: String::new(),
            client_cert_path: String::new(),
            client_key_path: String::new(),
            sysdba: false,
            oracle_connection_type: None,
            connection_string: None,
            redis_connection_mode: None,
            redis_sentinel_master: String::new(),
            redis_sentinel_nodes: String::new(),
            redis_sentinel_username: String::new(),
            redis_sentinel_password: String::new(),
            redis_sentinel_tls: false,
            redis_cluster_nodes: String::new(),
            redis_key_separator: crate::models::connection::default_redis_key_separator(),
            redis_scan_page_size: None,
            redis_database_aliases: Default::default(),
            redis_key_templates: Vec::new(),
            redis_key_grouping: None,
            etcd_endpoints: String::new(),
            gbase_server: String::new(),
            informix_server: String::new(),
            external_config: None,
            plugin_id: None,
            plugin_connection_provider: None,
            plugin_connection_type: None,
            connection_secrets: Default::default(),
            jdbc_driver_class: None,
            jdbc_driver_paths: Vec::new(),
            one_time: false,
            save_password: true,
            read_only: false,
            is_production: false,
            production_databases: vec![],
            database_info: None,
        }
    }

    fn ssh_hop(id: &str, password: &str, passphrase: &str) -> SshTunnelConfig {
        SshTunnelConfig {
            profile_id: String::new(),
            id: id.to_string(),
            name: String::new(),
            enabled: true,
            host: "bastion".to_string(),
            port: 22,
            user: "user".to_string(),
            password: password.to_string(),
            key_path: "~/.ssh/id_ed25519".to_string(),
            key_passphrase: passphrase.to_string(),
            connect_timeout_secs: 5,
            expose_lan: false,
            use_ssh_agent: false,
            ssh_agent_sock_path: String::new(),
            auth_method: "key".to_string(),
            allow_exec_channel_proxy: false,
        }
    }

    fn http_tunnel(id: &str, token: &str) -> TransportLayerConfig {
        TransportLayerConfig::HttpTunnel(HttpTunnelConfig {
            profile_id: String::new(),
            id: id.to_string(),
            name: String::new(),
            enabled: true,
            url: "https://dbx.example.com/dbx_tunnel.php".to_string(),
            token: token.to_string(),
            connect_timeout_secs: 10,
        })
    }

    fn read_configs(path: &Path) -> Vec<ConnectionConfig> {
        let json = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn save_connections_moves_passwords_to_secret_store_and_redacts_file() {
        let path = temp_connections_file("save-redacts");
        let store = MemorySecretStore::default();
        let mut config = connection("main", "db-secret", "ssh-secret");
        config.transport_layers = vec![TransportLayerConfig::Ssh(ssh_hop("hop-1", "hop-secret", "hop-key"))];
        config.redis_sentinel_password = "sentinel-secret".to_string();
        let configs = vec![config];

        save_connections_to_file(&path, &configs, &store).unwrap();

        assert_eq!(store.get_existing("main", MAIN_PASSWORD_KEY).as_deref(), Some("db-secret"));
        assert_eq!(store.get_existing("main", "transport_layers.hop-1.ssh_password").as_deref(), Some("hop-secret"));
        assert_eq!(store.get_existing("main", "transport_layers.hop-1.ssh_key_passphrase").as_deref(), Some("hop-key"));
        assert_eq!(store.get_existing("main", REDIS_SENTINEL_PASSWORD_KEY).as_deref(), Some("sentinel-secret"));
        let persisted = read_configs(&path);
        assert_eq!(persisted[0].password, "");
        match &persisted[0].transport_layers[0] {
            TransportLayerConfig::Ssh(ssh) => {
                assert_eq!(ssh.password, "");
                assert_eq!(ssh.key_passphrase, "");
            }
            _ => panic!("expected ssh layer"),
        }
        assert_eq!(persisted[0].redis_sentinel_password, "");
    }

    #[test]
    fn load_connections_restores_passwords_from_secret_store() {
        let path = temp_connections_file("load-restores");
        let store = MemorySecretStore::default();
        store.set_existing("main", MAIN_PASSWORD_KEY, "db-secret");
        store.set_existing("main", SSH_PASSWORD_KEY, "ssh-secret");
        store.set_existing("main", "ssh_tunnels.hop-1.password", "hop-secret");
        store.set_existing("main", "ssh_tunnels.hop-1.key_passphrase", "hop-key");
        store.set_existing("main", REDIS_SENTINEL_PASSWORD_KEY, "sentinel-secret");
        let mut sanitized_config = connection("main", "", "");
        sanitized_config.transport_layers = vec![TransportLayerConfig::Ssh(ssh_hop("hop-1", "", ""))];
        let sanitized = vec![sanitized_config];
        std::fs::write(&path, serde_json::to_string_pretty(&sanitized).unwrap()).unwrap();

        let loaded = load_connections_from_file(&path, &store).unwrap();

        assert_eq!(loaded[0].password, "db-secret");
        match &loaded[0].transport_layers[0] {
            TransportLayerConfig::Ssh(ssh) => {
                assert_eq!(ssh.password, "hop-secret");
                assert_eq!(ssh.key_passphrase, "hop-key");
            }
            _ => panic!("expected ssh layer"),
        }
        assert_eq!(loaded[0].redis_sentinel_password, "sentinel-secret");
    }

    #[test]
    fn save_and_load_connections_move_http_tunnel_token_to_secret_store() {
        let path = temp_connections_file("http-tunnel-token");
        let store = MemorySecretStore::default();
        let mut config = connection("main", "", "");
        config.transport_layers = vec![http_tunnel("http", "tunnel-secret")];

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(
            store.get_existing("main", "transport_layers.http.http_tunnel_token").as_deref(),
            Some("tunnel-secret")
        );
        let persisted = read_configs(&path);
        match &persisted[0].transport_layers[0] {
            TransportLayerConfig::HttpTunnel(http) => assert_eq!(http.token, ""),
            _ => panic!("expected http tunnel layer"),
        }

        let loaded = load_connections_from_file(&path, &store).unwrap();
        match &loaded[0].transport_layers[0] {
            TransportLayerConfig::HttpTunnel(http) => assert_eq!(http.token, "tunnel-secret"),
            _ => panic!("expected http tunnel layer"),
        }
    }

    #[test]
    fn load_connections_migrates_plaintext_passwords_and_rewrites_sanitized_file() {
        let path = temp_connections_file("migrates-plaintext");
        let store = MemorySecretStore::default();
        let legacy = serde_json::json!([{
            "id": "legacy",
            "name": "legacy connection",
            "db_type": "postgres",
            "host": "localhost",
            "port": 5432,
            "username": "postgres",
            "password": "plain-db",
            "database": "postgres",
            "ssh_enabled": true,
            "ssh_host": "bastion",
            "ssh_port": 22,
            "ssh_user": "user",
            "ssh_password": "plain-ssh"
        }]);
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let loaded = load_connections_from_file(&path, &store).unwrap();

        assert_eq!(loaded[0].password, "plain-db");
        match &loaded[0].transport_layers[0] {
            TransportLayerConfig::Ssh(ssh) => assert_eq!(ssh.password, "plain-ssh"),
            _ => panic!("expected ssh layer"),
        }
        assert_eq!(store.get_existing("legacy", MAIN_PASSWORD_KEY).as_deref(), Some("plain-db"));
        assert_eq!(store.get_existing("legacy", "transport_layers.legacy.ssh_password").as_deref(), Some("plain-ssh"));
        let persisted = read_configs(&path);
        assert_eq!(persisted[0].password, "");
        match &persisted[0].transport_layers[0] {
            TransportLayerConfig::Ssh(ssh) => assert_eq!(ssh.password, ""),
            _ => panic!("expected ssh layer"),
        }
    }

    #[test]
    fn save_connections_deletes_secrets_for_removed_connections() {
        let path = temp_connections_file("deletes-removed");
        let store = MemorySecretStore::default();
        let previous = vec![connection("old", "", ""), connection("kept", "", "")];
        std::fs::write(&path, serde_json::to_string_pretty(&previous).unwrap()).unwrap();
        store.set_existing("old", MAIN_PASSWORD_KEY, "old-db");
        store.set_existing("old", SSH_PASSWORD_KEY, "old-ssh");
        store.set_existing("kept", MAIN_PASSWORD_KEY, "kept-db");

        save_connections_to_file(&path, &[connection("kept", "new-db", "")], &store).unwrap();

        assert!(store.was_deleted("old", MAIN_PASSWORD_KEY));
        assert!(store.was_deleted("old", SSH_PASSWORD_KEY));
        assert_eq!(store.get_existing("kept", MAIN_PASSWORD_KEY).as_deref(), Some("new-db"));
    }

    #[test]
    fn save_connections_moves_connection_string_to_secret_store_and_restores_it() {
        let path = temp_connections_file("connection-string");
        let store = MemorySecretStore::default();
        let mut config = connection("mongo", "", "");
        config.db_type = DatabaseType::MongoDb;
        config.connection_string = Some("mongodb://user:secret@localhost/app".to_string());

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(
            store.get_existing("mongo", CONNECTION_STRING_KEY).as_deref(),
            Some("mongodb://user:secret@localhost/app")
        );
        let persisted = read_configs(&path);
        assert_eq!(persisted[0].connection_string, None);

        let loaded = load_connections_from_file(&path, &store).unwrap();
        assert_eq!(loaded[0].connection_string.as_deref(), Some("mongodb://user:secret@localhost/app"));
    }

    #[test]
    fn save_connections_moves_init_script_to_secret_store_and_restores_it() {
        let path = temp_connections_file("init-script");
        let store = MemorySecretStore::default();
        let mut config = connection("duck", "", "");
        config.db_type = DatabaseType::DuckDb;
        config.init_script = Some("CREATE SECRET (TYPE quack, TOKEN 'token-value');".to_string());

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(
            store.get_existing("duck", INIT_SCRIPT_KEY).as_deref(),
            Some("CREATE SECRET (TYPE quack, TOKEN 'token-value');")
        );
        let persisted = read_configs(&path);
        assert_eq!(persisted[0].init_script, None);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("token-value"));

        let loaded = load_connections_from_file(&path, &store).unwrap();
        assert_eq!(loaded[0].init_script.as_deref(), Some("CREATE SECRET (TYPE quack, TOKEN 'token-value');"));
    }

    #[test]
    fn save_connections_moves_mq_auth_secrets_to_secret_store_and_restores_them() {
        let path = temp_connections_file("mq-auth");
        let store = MemorySecretStore::default();
        let mut config = connection("pulsar", "", "");
        config.db_type = DatabaseType::MessageQueue;
        config.external_config = Some(serde_json::json!({
            "systemKind": "pulsar",
            "adminUrl": "http://localhost:8080",
            "auth": {
                "kind": "token",
                "token": "mq-token-secret"
            }
        }));

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(store.get_existing("pulsar", "mq.auth.token").as_deref(), Some("mq-token-secret"));
        let persisted_json = std::fs::read_to_string(&path).unwrap();
        assert!(!persisted_json.contains("mq-token-secret"));

        let loaded = load_connections_from_file(&path, &store).unwrap();
        let auth = loaded[0].external_config.as_ref().and_then(|value| value.get("auth")).expect("restored MQ auth");
        assert_eq!(auth.get("token").and_then(serde_json::Value::as_str), Some("mq-token-secret"));
    }

    #[test]
    fn save_connections_moves_cassandra_store_passwords_to_secret_store_and_restores_them() {
        let path = temp_connections_file("cassandra-tls");
        let store = MemorySecretStore::default();
        let mut config = connection("cassandra", "", "");
        config.db_type = DatabaseType::Cassandra;
        config.external_config = Some(serde_json::json!({
            "tls": {
                "truststore_path": "/certs/client.truststore",
                "truststore_password": "trust-secret",
                "keystore_path": "/certs/client.keystore",
                "keystore_password": "key-secret"
            }
        }));

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(store.get_existing("cassandra", CASSANDRA_TRUSTSTORE_PASSWORD_KEY).as_deref(), Some("trust-secret"));
        assert_eq!(store.get_existing("cassandra", CASSANDRA_KEYSTORE_PASSWORD_KEY).as_deref(), Some("key-secret"));
        let persisted_json = std::fs::read_to_string(&path).unwrap();
        assert!(!persisted_json.contains("trust-secret"));
        assert!(!persisted_json.contains("key-secret"));

        let loaded = load_connections_from_file(&path, &store).unwrap();
        let tls = loaded[0].external_config.as_ref().and_then(|value| value.get("tls")).unwrap();
        assert_eq!(tls.get("truststore_password").and_then(serde_json::Value::as_str), Some("trust-secret"));
        assert_eq!(tls.get("keystore_password").and_then(serde_json::Value::as_str), Some("key-secret"));
    }

    #[test]
    fn save_connections_moves_mq_basic_and_oauth_secrets_to_secret_store_and_restores_them() {
        let path = temp_connections_file("mq-auth-multiple");
        let store = MemorySecretStore::default();
        let mut basic = connection("basic", "", "");
        basic.db_type = DatabaseType::MessageQueue;
        basic.external_config = Some(serde_json::json!({
            "systemKind": "pulsar",
            "adminUrl": "http://localhost:8080",
            "auth": {
                "kind": "basic",
                "username": "admin",
                "password": "basic-secret"
            }
        }));
        let mut oauth = connection("oauth", "", "");
        oauth.db_type = DatabaseType::MessageQueue;
        oauth.external_config = Some(serde_json::json!({
            "systemKind": "pulsar",
            "adminUrl": "http://localhost:8080",
            "auth": {
                "kind": "oauth2",
                "issuerUrl": "https://issuer/token",
                "clientId": "client",
                "clientSecret": "oauth-secret"
            }
        }));

        save_connections_to_file(&path, &[basic, oauth], &store).unwrap();

        assert_eq!(store.get_existing("basic", "mq.auth.password").as_deref(), Some("basic-secret"));
        assert_eq!(store.get_existing("oauth", "mq.auth.client_secret").as_deref(), Some("oauth-secret"));
        let persisted_json = std::fs::read_to_string(&path).unwrap();
        assert!(!persisted_json.contains("basic-secret"));
        assert!(!persisted_json.contains("oauth-secret"));

        let loaded = load_connections_from_file(&path, &store).unwrap();
        let basic_auth = loaded[0].external_config.as_ref().and_then(|value| value.get("auth")).unwrap();
        let oauth_auth = loaded[1].external_config.as_ref().and_then(|value| value.get("auth")).unwrap();
        assert_eq!(basic_auth.get("password").and_then(serde_json::Value::as_str), Some("basic-secret"));
        assert_eq!(oauth_auth.get("clientSecret").and_then(serde_json::Value::as_str), Some("oauth-secret"));
    }

    #[test]
    fn save_connections_preserves_existing_mq_secret_when_config_is_sanitized() {
        let path = temp_connections_file("mq-auth-preserve");
        let store = MemorySecretStore::default();
        store.set_existing("pulsar", "mq.auth.token", "existing-token");
        let mut config = connection("pulsar", "", "");
        config.db_type = DatabaseType::MessageQueue;
        config.external_config = Some(serde_json::json!({
            "systemKind": "pulsar",
            "adminUrl": "http://localhost:8080",
            "auth": {
                "kind": "token",
                "token": ""
            }
        }));

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(store.get_existing("pulsar", "mq.auth.token").as_deref(), Some("existing-token"));
        let loaded = load_connections_from_file(&path, &store).unwrap();
        let auth = loaded[0].external_config.as_ref().and_then(|value| value.get("auth")).unwrap();
        assert_eq!(auth.get("token").and_then(serde_json::Value::as_str), Some("existing-token"));
    }

    #[test]
    fn save_connections_deletes_stale_mq_auth_secrets_when_kind_changes() {
        let path = temp_connections_file("mq-auth-kind-change");
        let store = MemorySecretStore::default();
        store.set_existing("pulsar", MQ_AUTH_TOKEN_KEY, "old-token");
        let mut config = connection("pulsar", "", "");
        config.db_type = DatabaseType::MessageQueue;
        config.external_config = Some(serde_json::json!({
            "systemKind": "pulsar",
            "adminUrl": "http://localhost:8080",
            "auth": {
                "kind": "basic",
                "username": "admin",
                "password": "basic-secret"
            }
        }));

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(store.get_existing("pulsar", MQ_AUTH_TOKEN_KEY), None);
        assert_eq!(store.get_existing("pulsar", MQ_AUTH_PASSWORD_KEY).as_deref(), Some("basic-secret"));
    }

    #[test]
    fn save_connections_moves_mq_token_signing_key_to_secret_store_and_restores_it() {
        let path = temp_connections_file("mq-token-signing");
        let store = MemorySecretStore::default();
        let mut config = connection("pulsar", "", "");
        config.db_type = DatabaseType::MessageQueue;
        config.external_config = Some(serde_json::json!({
            "systemKind": "pulsar",
            "adminUrl": "http://localhost:8080",
            "auth": { "kind": "none" },
            "tokenSigning": {
                "algorithm": "hs256",
                "key": "broker-signing-secret"
            }
        }));

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(store.get_existing("pulsar", MQ_TOKEN_SIGNING_KEY).as_deref(), Some("broker-signing-secret"));
        let persisted_json = std::fs::read_to_string(&path).unwrap();
        assert!(!persisted_json.contains("broker-signing-secret"));

        let loaded = load_connections_from_file(&path, &store).unwrap();
        let signing = loaded[0].external_config.as_ref().and_then(|value| value.get("tokenSigning")).unwrap();
        assert_eq!(signing.get("key").and_then(serde_json::Value::as_str), Some("broker-signing-secret"));
    }

    // ── MQTT 密钥持久化测试 ────────────────────────────────────────

    #[test]
    fn save_connections_moves_mqtt_password_to_secret_store_and_redacts_file() {
        let path = temp_connections_file("mqtt-auth");
        let store = MemorySecretStore::default();
        let mut config = connection("mqtt-broker", "", "");
        config.db_type = DatabaseType::Mqtt;
        config.external_config = Some(serde_json::json!({
            "host": "localhost",
            "port": 1883,
            "clientId": "dbx-test",
            "protocolVersion": "v5",
            "transport": "tcp",
            "tls": false,
            "tlsSkipVerify": false,
            "auth": {
                "kind": "password",
                "username": "admin",
                "password": "mqtt-secret"
            },
            "keepAliveSecs": 60,
            "connectTimeoutSecs": 30
        }));

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(store.get_existing("mqtt-broker", MQTT_AUTH_PASSWORD_KEY).as_deref(), Some("mqtt-secret"));
        let persisted_json = std::fs::read_to_string(&path).unwrap();
        assert!(!persisted_json.contains("mqtt-secret"));
    }

    #[test]
    fn load_connections_restores_mqtt_password_from_secret_store() {
        let path = temp_connections_file("mqtt-load");
        let store = MemorySecretStore::default();
        store.set_existing("mqtt-broker", MQTT_AUTH_PASSWORD_KEY, "mqtt-secret");
        let sanitized = vec![{
            let mut c = connection("mqtt-broker", "", "");
            c.db_type = DatabaseType::Mqtt;
            c.external_config = Some(serde_json::json!({
                "host": "localhost",
                "port": 1883,
                "clientId": "dbx-test",
                "protocolVersion": "v5",
                "transport": "tcp",
                "tls": false,
                "tlsSkipVerify": false,
                "auth": {
                    "kind": "password",
                    "username": "admin",
                    "password": ""
                },
                "keepAliveSecs": 60,
                "connectTimeoutSecs": 30
            }));
            c
        }];
        std::fs::write(&path, serde_json::to_string_pretty(&sanitized).unwrap()).unwrap();

        let loaded = load_connections_from_file(&path, &store).unwrap();
        let auth = loaded[0].external_config.as_ref().and_then(|v| v.get("auth")).unwrap();
        assert_eq!(auth.get("password").and_then(serde_json::Value::as_str), Some("mqtt-secret"));
    }

    #[test]
    fn load_connections_migrates_plaintext_mqtt_password_and_rewrites_sanitized_file() {
        let path = temp_connections_file("mqtt-migrate");
        let store = MemorySecretStore::default();
        let legacy = serde_json::json!([{
            "id": "mqtt-legacy",
            "name": "legacy mqtt",
            "db_type": "mqtt",
            "host": "localhost",
            "port": 1883,
            "username": "",
            "password": "",
            "external_config": {
                "host": "localhost",
                "port": 1883,
                "clientId": "dbx-legacy",
                "protocolVersion": "v5",
                "transport": "tcp",
                "tls": false,
                "tlsSkipVerify": false,
                "auth": {
                    "kind": "password",
                    "username": "admin",
                    "password": "plain-mqtt-password"
                },
                "keepAliveSecs": 60,
                "connectTimeoutSecs": 30
            }
        }]);
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let loaded = load_connections_from_file(&path, &store).unwrap();
        let auth = loaded[0].external_config.as_ref().and_then(|v| v.get("auth")).unwrap();
        assert_eq!(auth.get("password").and_then(serde_json::Value::as_str), Some("plain-mqtt-password"));
        assert_eq!(store.get_existing("mqtt-legacy", MQTT_AUTH_PASSWORD_KEY).as_deref(), Some("plain-mqtt-password"));

        let persisted_json = std::fs::read_to_string(&path).unwrap();
        assert!(!persisted_json.contains("plain-mqtt-password"));
    }

    #[test]
    fn save_connections_preserves_existing_mqtt_secret_when_config_is_sanitized() {
        let path = temp_connections_file("mqtt-preserve");
        let store = MemorySecretStore::default();
        store.set_existing("mqtt-broker", MQTT_AUTH_PASSWORD_KEY, "existing-mqtt-password");
        let mut config = connection("mqtt-broker", "", "");
        config.db_type = DatabaseType::Mqtt;
        config.external_config = Some(serde_json::json!({
            "host": "localhost",
            "port": 1883,
            "clientId": "dbx-test",
            "protocolVersion": "v5",
            "transport": "tcp",
            "tls": false,
            "tlsSkipVerify": false,
            "auth": {
                "kind": "password",
                "username": "admin",
                "password": ""
            },
            "keepAliveSecs": 60,
            "connectTimeoutSecs": 30
        }));

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(
            store.get_existing("mqtt-broker", MQTT_AUTH_PASSWORD_KEY).as_deref(),
            Some("existing-mqtt-password")
        );
        let loaded = load_connections_from_file(&path, &store).unwrap();
        let auth = loaded[0].external_config.as_ref().and_then(|v| v.get("auth")).unwrap();
        assert_eq!(auth.get("password").and_then(serde_json::Value::as_str), Some("existing-mqtt-password"));
    }

    #[test]
    fn mqtt_none_auth_does_not_store_password() {
        let path = temp_connections_file("mqtt-none-auth");
        let store = MemorySecretStore::default();
        let mut config = connection("mqtt-none", "", "");
        config.db_type = DatabaseType::Mqtt;
        config.external_config = Some(serde_json::json!({
            "host": "localhost",
            "port": 1883,
            "clientId": "dbx-test",
            "protocolVersion": "v5",
            "transport": "tcp",
            "tls": false,
            "tlsSkipVerify": false,
            "auth": {
                "kind": "none"
            },
            "keepAliveSecs": 60,
            "connectTimeoutSecs": 30
        }));

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(store.get_existing("mqtt-none", MQTT_AUTH_PASSWORD_KEY), None);
    }

    #[test]
    fn mqtt_auth_kind_change_from_password_to_none_clears_stored_secret() {
        let path = temp_connections_file("mqtt-kind-change");
        let store = MemorySecretStore::default();
        store.set_existing("mqtt-broker", MQTT_AUTH_PASSWORD_KEY, "old-mqtt-password");
        let mut config = connection("mqtt-broker", "", "");
        config.db_type = DatabaseType::Mqtt;
        config.external_config = Some(serde_json::json!({
            "host": "localhost",
            "port": 1883,
            "clientId": "dbx-test",
            "protocolVersion": "v5",
            "transport": "tcp",
            "tls": false,
            "tlsSkipVerify": false,
            "auth": {
                "kind": "none"
            },
            "keepAliveSecs": 60,
            "connectTimeoutSecs": 30
        }));

        save_connections_to_file(&path, &[config], &store).unwrap();

        assert_eq!(store.get_existing("mqtt-broker", MQTT_AUTH_PASSWORD_KEY), None);
    }

    #[test]
    fn save_connections_moves_plugin_secrets_to_secret_store_and_restores_them() {
        let path = temp_connections_file("plugin-connection-secret");
        let store = MemorySecretStore::default();
        let mut config = connection("plugin-connection", "", "");
        config.db_type = DatabaseType::Plugin;
        config.plugin_id = Some("dbx.example.hello".to_string());
        config.plugin_connection_provider = Some("hello.connection".to_string());
        config.plugin_connection_type = Some("hello".to_string());
        config.external_config = Some(serde_json::json!({ "greeting": "Hello" }));
        config.connection_secrets.insert("access_token".to_string(), "plugin-secret".to_string());

        save_connections_to_file(&path, &[config], &store).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("plugin-secret"));
        let persisted: Vec<ConnectionConfig> = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted[0].connection_secrets.get("access_token").map(String::as_str), Some(""));
        assert_eq!(
            store
                .get_existing("plugin-connection", &format!("{PLUGIN_CONNECTION_SECRET_PREFIX}access_token"))
                .as_deref(),
            Some("plugin-secret")
        );

        let loaded = load_connections_from_file(&path, &store).unwrap();
        assert_eq!(loaded[0].connection_secrets.get("access_token").map(String::as_str), Some("plugin-secret"));
    }

    #[test]
    fn load_connections_preserves_opaque_null_plugin_secrets() {
        let path = temp_connections_file("plugin-null-secret");
        let store = MemorySecretStore::default();
        let mut config = connection("plugin-connection", "", "");
        config.db_type = DatabaseType::Plugin;
        config.plugin_id = Some("io.dbx.ssh".to_string());
        config.plugin_connection_provider = Some("ssh.connection".to_string());
        config.plugin_connection_type = Some("ssh".to_string());
        config.external_config = Some(serde_json::json!({ "sudo_source": "custom" }));
        config.connection_secrets.insert("sudo_password".to_string(), "null".to_string());
        config.connection_secrets.insert("totp_secret".to_string(), "".to_string());
        config.connection_secrets.insert("private_key_passphrase".to_string(), "real-passphrase".to_string());

        save_connections_to_file(&path, &[config], &store).unwrap();
        store.set_existing("plugin-connection", &format!("{PLUGIN_CONNECTION_SECRET_PREFIX}totp_secret"), "null");
        let stored = store.values.borrow().clone();
        let deleted = store.deleted.borrow().clone();
        let raw_config = std::fs::read(&path).unwrap();

        let loaded = load_connections_from_file(&path, &store).unwrap();
        let secrets = &loaded[0].connection_secrets;
        assert_eq!(secrets.get("sudo_password").map(String::as_str), Some("null"));
        assert_eq!(secrets.get("totp_secret").map(String::as_str), Some("null"));
        assert_eq!(secrets.get("private_key_passphrase").map(String::as_str), Some("real-passphrase"));
        assert_eq!(load_connections_from_file(&path, &store).unwrap()[0].connection_secrets, *secrets);
        assert_eq!(*store.values.borrow(), stored);
        assert_eq!(*store.deleted.borrow(), deleted);
        assert_eq!(std::fs::read(&path).unwrap(), raw_config);
        save_connections_to_file(&path, &loaded, &store).unwrap();
        assert_eq!(load_connections_from_file(&path, &store).unwrap()[0].connection_secrets, *secrets);
        assert_eq!(*store.values.borrow(), stored);
    }

    #[test]
    fn client_redaction_removes_every_secret_and_lists_saved_paths() {
        let mut config = connection("prod", "db-secret", "");
        config.url_params = Some("sslmode=require&password=url-secret;apiKey=k".to_string());
        config.connection_string = Some("Server=x;Password=cs".to_string());
        config.init_script = Some("CREATE SECRET s (KEY_ID 'a', SECRET 'b')".to_string());
        config.transport_layers = vec![
            TransportLayerConfig::Ssh(ssh_hop("hop-a", "ssh-secret", "pp-value")),
            http_tunnel("", "tunnel-token"),
        ];
        config.connection_secrets.insert("token".to_string(), "plugin-secret".to_string());

        let redacted = super::redact_connection_for_client(&config).unwrap();
        let text = redacted.to_string();
        for secret in
            ["db-secret", "url-secret", "ssh-secret", "pp-value", "tunnel-token", "plugin-secret", "Password=cs"]
        {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert_eq!(redacted["url_params"], "sslmode=require&password=;apiKey=");
        assert!(redacted["connection_string"].is_null());
        assert!(redacted.get("connection_secrets").is_none());
        let saved =
            redacted["saved_secrets"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>();
        assert_eq!(
            saved,
            vec![
                "password",
                "connection_string",
                "init_script",
                "transport_layers.hop-a.password",
                "transport_layers.hop-a.key_passphrase",
                "transport_layers.#1.token",
                "url_params",
                "connection_secrets.token",
            ]
        );
    }

    #[test]
    fn client_merge_keeps_stored_secrets_for_blank_fields_and_honors_clears() {
        let mut stored = connection("prod", "db-secret", "");
        stored.url_params = Some("sslmode=require&password=url-secret".to_string());
        stored.init_script = Some("SET x = 1".to_string());
        stored.transport_layers = vec![TransportLayerConfig::Ssh(ssh_hop("hop-a", "ssh-secret", "pp-value"))];
        stored.connection_secrets.insert("token".to_string(), "plugin-secret".to_string());

        let redacted = super::redact_connection_for_client(&stored).unwrap();
        let mut incoming = super::ClientConnectionInput::from_value(redacted).unwrap().config;
        incoming.host = "db.internal".to_string();
        incoming.url_params = Some("sslmode=disable&password=".to_string());
        let merged = super::merge_stored_connection_secrets(&incoming, Some(&stored), &[]).unwrap();
        assert_eq!(merged.host, "db.internal");
        assert_eq!(merged.password, "db-secret");
        assert_eq!(merged.url_params.as_deref(), Some("sslmode=disable&password=url-secret"));
        assert_eq!(merged.init_script.as_deref(), Some("SET x = 1"));
        let TransportLayerConfig::Ssh(ssh) = &merged.transport_layers[0] else { panic!("ssh layer expected") };
        assert_eq!((ssh.password.as_str(), ssh.key_passphrase.as_str()), ("ssh-secret", "pp-value"));
        assert_eq!(merged.connection_secrets.get("token").map(String::as_str), Some("plugin-secret"));

        incoming.password = "typed".to_string();
        let cleared = vec![
            "transport_layers.hop-a.key_passphrase".to_string(),
            "init_script".to_string(),
            "connection_secrets.token".to_string(),
        ];
        let merged = super::merge_stored_connection_secrets(&incoming, Some(&stored), &cleared).unwrap();
        assert_eq!(merged.password, "typed");
        assert_eq!(merged.init_script, None);
        assert!(merged.connection_secrets.is_empty());
        let TransportLayerConfig::Ssh(ssh) = &merged.transport_layers[0] else { panic!("ssh layer expected") };
        assert_eq!((ssh.password.as_str(), ssh.key_passphrase.as_str()), ("ssh-secret", ""));
    }

    #[test]
    fn client_merge_never_reuses_the_password_of_a_no_save_connection() {
        let stored = connection("prod", "db-secret", "");
        let mut incoming = connection("prod", "", "");
        incoming.save_password = false;
        let merged = super::merge_stored_connection_secrets(&incoming, Some(&stored), &[]).unwrap();
        assert_eq!(merged.password, "");
    }

    #[test]
    fn client_merge_does_not_move_plugin_secrets_to_another_provider() {
        let mut stored = connection("plugin", "", "");
        stored.plugin_id = Some("plugin-a".to_string());
        stored.connection_secrets.insert("token".to_string(), "plugin-secret".to_string());
        let mut incoming = stored.clone();
        incoming.connection_secrets.clear();
        incoming.plugin_id = Some("plugin-b".to_string());
        let merged = super::merge_stored_connection_secrets(&incoming, Some(&stored), &[]).unwrap();
        assert!(merged.connection_secrets.is_empty());
    }

    #[test]
    fn client_input_strips_directive_fields() {
        let mut value = serde_json::to_value(connection("prod", "", "")).unwrap();
        value["saved_secrets"] = serde_json::json!(["password"]);
        value["cleared_secrets"] = serde_json::json!(["init_script"]);
        value["secrets_from_connection_id"] = serde_json::json!("source");
        let input: super::ClientConnectionInput = serde_json::from_value(value).unwrap();
        assert_eq!(input.cleared_secrets, vec!["init_script".to_string()]);
        assert_eq!(input.secrets_from_connection_id.as_deref(), Some("source"));
        assert_eq!(input.config.id, "prod");
    }
}
