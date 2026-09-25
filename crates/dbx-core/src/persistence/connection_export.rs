//! Encrypted connection export files and the key-derivation primitives shared
//! with cloud sync.
//!
//! Format history (`format: "dbx-encrypted"`):
//! - version 1: written by the frontend with PBKDF2-HMAC-SHA256 (100 000
//!   iterations) and AES-256-GCM, fields `salt`, `iv`, `data`. Import only.
//! - version 2: written by the backend with Argon2id (parameters stored in the
//!   `kdf` header) and AES-256-GCM, the header authenticated as associated
//!   data, fields `kdf`, `cipher`, `nonce`, `data`.

use aes_gcm::{
    aead::{rand_core::RngCore, Aead, OsRng, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::connection_secrets::{merge_stored_connection_secrets, ClientConnectionInput};
use crate::models::connection::TransportLayerConfig;

pub const EXPORT_FORMAT: &str = "dbx-encrypted";
pub const EXPORT_VERSION_PBKDF2: u64 = 1;
pub const EXPORT_VERSION_ARGON2: u64 = 2;
/// Minimum passphrase length (in characters) for newly written exports.
pub const MIN_EXPORT_PASSPHRASE_CHARS: usize = 12;
const LEGACY_PBKDF2_ITERATIONS: u32 = 100_000;
const CIPHER_NAME: &str = "aes-256-gcm";
const KDF_NAME: &str = "argon2id";
const WRONG_PASSPHRASE: &str = "wrong_passphrase";

/// Argon2id cost parameters stored next to every value derived from a user
/// passphrase so they can be raised later without breaking old files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Argon2Params {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Argon2Params {
    /// Parameters for new passphrase-protected data: 64 MiB, 3 passes.
    pub const STRONG: Self = Self { memory_kib: 64 * 1024, iterations: 3, parallelism: 1 };
    /// Parameters of data written before the parameters were recorded.
    pub const LEGACY: Self = Self { memory_kib: 19 * 1024, iterations: 2, parallelism: 1 };

    /// Parameters for newly written data. Unit tests of this crate run with
    /// unoptimized dependencies, so they use the minimum accepted cost; the
    /// recorded parameters make both readable everywhere.
    pub fn for_new_data() -> Self {
        if cfg!(test) {
            Self { memory_kib: 8 * 1024, iterations: 1, parallelism: 1 }
        } else {
            Self::STRONG
        }
    }

    /// Rejects parameters outside a sane range so an untrusted file cannot
    /// make the process allocate unbounded memory or spin for minutes.
    pub fn validate(self) -> Result<Self, String> {
        if !(8 * 1024..=1024 * 1024).contains(&self.memory_kib)
            || !(1..=16).contains(&self.iterations)
            || !(1..=16).contains(&self.parallelism)
        {
            return Err("Unsupported key derivation parameters".to_string());
        }
        Ok(self)
    }

    pub fn derive_key(self, passphrase: &[u8], salt: &[u8]) -> Result<[u8; 32], String> {
        let params = Params::new(self.memory_kib, self.iterations, self.parallelism, Some(32))
            .map_err(|error| error.to_string())?;
        let mut key = [0u8; 32];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(passphrase, salt, &mut key)
            .map_err(|error| error.to_string())?;
        Ok(key)
    }
}

/// HMAC-SHA256 (RFC 2104) with precomputed pads, used for PBKDF2 and for
/// sync snapshot integrity.
#[derive(Clone)]
pub(crate) struct HmacSha256 {
    inner: Sha256,
    outer: Sha256,
}

impl HmacSha256 {
    pub(crate) fn new(key: &[u8]) -> Self {
        let mut block = [0u8; 64];
        if key.len() > block.len() {
            block[..32].copy_from_slice(&Sha256::digest(key));
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let mut inner_pad = [0x36u8; 64];
        let mut outer_pad = [0x5cu8; 64];
        for index in 0..64 {
            inner_pad[index] ^= block[index];
            outer_pad[index] ^= block[index];
        }
        let mut inner = Sha256::new();
        inner.update(inner_pad);
        let mut outer = Sha256::new();
        outer.update(outer_pad);
        Self { inner, outer }
    }

    pub(crate) fn mac(&self, parts: &[&[u8]]) -> [u8; 32] {
        let mut inner = self.inner.clone();
        for part in parts {
            inner.update(part);
        }
        let inner_hash = inner.finalize();
        let mut outer = self.outer.clone();
        outer.update(inner_hash);
        let mut output = [0u8; 32];
        output.copy_from_slice(&outer.finalize());
        output
    }
}

/// Constant-time comparison of two MACs.
pub(crate) fn mac_equals(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// PBKDF2-HMAC-SHA256 producing one 32-byte block (RFC 8018).
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let prf = HmacSha256::new(password);
    let mut block = prf.mac(&[salt, &1u32.to_be_bytes()]);
    let mut output = block;
    for _ in 1..iterations {
        block = prf.mac(&[&block]);
        for (output, byte) in output.iter_mut().zip(block) {
            *output ^= byte;
        }
    }
    output
}

pub fn validate_new_export_passphrase(passphrase: &str) -> Result<(), String> {
    if passphrase.chars().count() < MIN_EXPORT_PASSPHRASE_CHARS {
        return Err(format!(
            "passphrase_too_short: the export passphrase must be at least {MIN_EXPORT_PASSPHRASE_CHARS} characters"
        ));
    }
    Ok(())
}

fn v2_associated_data(params: Argon2Params) -> String {
    format!(
        "{EXPORT_FORMAT}|{EXPORT_VERSION_ARGON2}|{KDF_NAME}|{}|{}|{}|{CIPHER_NAME}",
        params.memory_kib, params.iterations, params.parallelism
    )
}

/// Encrypts an export bundle (plaintext JSON) into a version 2 file object.
pub fn encrypt_connection_export(plaintext: &str, passphrase: &str) -> Result<serde_json::Value, String> {
    validate_new_export_passphrase(passphrase)?;
    encrypt_connection_export_with_params(plaintext, passphrase, Argon2Params::for_new_data())
}

fn encrypt_connection_export_with_params(
    plaintext: &str,
    passphrase: &str,
    params: Argon2Params,
) -> Result<serde_json::Value, String> {
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);
    let key = params.derive_key(passphrase.as_bytes(), &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|error| error.to_string())?;
    let aad = v2_associated_data(params);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext.as_bytes(), aad: aad.as_bytes() })
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "format": EXPORT_FORMAT,
        "version": EXPORT_VERSION_ARGON2,
        "kdf": {
            "name": KDF_NAME,
            "memoryKib": params.memory_kib,
            "iterations": params.iterations,
            "parallelism": params.parallelism,
            "salt": BASE64.encode(salt),
        },
        "cipher": CIPHER_NAME,
        "nonce": BASE64.encode(nonce),
        "data": BASE64.encode(ciphertext),
    }))
}

fn string_field<'a>(payload: &'a serde_json::Value, field: &str) -> Result<&'a str, String> {
    payload.get(field).and_then(serde_json::Value::as_str).ok_or_else(|| WRONG_PASSPHRASE.to_string())
}

fn decode_field(payload: &serde_json::Value, field: &str) -> Result<Vec<u8>, String> {
    BASE64.decode(string_field(payload, field)?).map_err(|_| WRONG_PASSPHRASE.to_string())
}

/// Decrypts a version 1 or 2 export file. Any authentication failure is
/// reported as `wrong_passphrase`.
pub fn decrypt_connection_export(payload: &serde_json::Value, passphrase: &str) -> Result<String, String> {
    if payload.get("format").and_then(serde_json::Value::as_str) != Some(EXPORT_FORMAT) {
        return Err("Unsupported encrypted config format".to_string());
    }
    let plaintext = match payload.get("version").and_then(serde_json::Value::as_u64) {
        Some(EXPORT_VERSION_PBKDF2) => {
            let salt = decode_field(payload, "salt")?;
            let iv = decode_field(payload, "iv")?;
            let ciphertext = decode_field(payload, "data")?;
            if iv.len() != 12 {
                return Err(WRONG_PASSPHRASE.to_string());
            }
            let key = pbkdf2_sha256(passphrase.as_bytes(), &salt, LEGACY_PBKDF2_ITERATIONS);
            let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| WRONG_PASSPHRASE.to_string())?;
            cipher.decrypt(Nonce::from_slice(&iv), ciphertext.as_ref()).map_err(|_| WRONG_PASSPHRASE.to_string())?
        }
        Some(EXPORT_VERSION_ARGON2) => {
            let kdf = payload.get("kdf").ok_or_else(|| "Unsupported encrypted config format".to_string())?;
            if kdf.get("name").and_then(serde_json::Value::as_str) != Some(KDF_NAME)
                || payload.get("cipher").and_then(serde_json::Value::as_str) != Some(CIPHER_NAME)
            {
                return Err("Unsupported encrypted config format".to_string());
            }
            let params: Argon2Params =
                serde_json::from_value(kdf.clone()).map_err(|_| "Unsupported encrypted config format".to_string())?;
            let params = params.validate()?;
            let salt = decode_field(kdf, "salt")?;
            let nonce = decode_field(payload, "nonce")?;
            let ciphertext = decode_field(payload, "data")?;
            if nonce.len() != 12 || salt.len() < 16 {
                return Err(WRONG_PASSPHRASE.to_string());
            }
            let key = params.derive_key(passphrase.as_bytes(), &salt)?;
            let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| WRONG_PASSPHRASE.to_string())?;
            let aad = v2_associated_data(params);
            cipher
                .decrypt(Nonce::from_slice(&nonce), Payload { msg: ciphertext.as_ref(), aad: aad.as_bytes() })
                .map_err(|_| WRONG_PASSPHRASE.to_string())?
        }
        _ => return Err("Unsupported encrypted config format".to_string()),
    };
    String::from_utf8(plaintext).map_err(|_| WRONG_PASSPHRASE.to_string())
}

/// Runs [`decrypt_connection_export`] on the blocking pool: key derivation is
/// deliberately expensive and must not stall an async worker.
pub async fn decrypt_connection_export_blocking(
    payload: serde_json::Value,
    passphrase: String,
) -> Result<String, String> {
    tokio::task::spawn_blocking(move || decrypt_connection_export(&payload, &passphrase))
        .await
        .map_err(|error| error.to_string())?
}

pub async fn encrypt_connection_export_blocking(
    plaintext: String,
    passphrase: String,
) -> Result<serde_json::Value, String> {
    tokio::task::spawn_blocking(move || encrypt_connection_export(&plaintext, &passphrase))
        .await
        .map_err(|error| error.to_string())?
}

fn fill_missing_tunnel_secrets(profile: &mut TransportLayerConfig, stored: &TransportLayerConfig) {
    match (profile, stored) {
        (TransportLayerConfig::Ssh(current), TransportLayerConfig::Ssh(stored)) => {
            if current.password.is_empty() {
                current.password = stored.password.clone();
            }
            if current.key_passphrase.is_empty() {
                current.key_passphrase = stored.key_passphrase.clone();
            }
        }
        (TransportLayerConfig::Proxy(current), TransportLayerConfig::Proxy(stored)) if current.password.is_empty() => {
            current.password = stored.password.clone();
        }
        (TransportLayerConfig::HttpTunnel(current), TransportLayerConfig::HttpTunnel(stored))
            if current.token.is_empty() =>
        {
            current.token = stored.token.clone();
        }
        _ => {}
    }
}

impl crate::storage::Storage {
    /// Produces the encrypted export file for a bundle built by the client
    /// from redacted configurations (`{ connections, layout?, tunnelProfiles? }`).
    /// Stored secrets are added back per connection/tunnel profile id before
    /// encryption, so they never pass through the client.
    pub async fn export_connections_encrypted(
        &self,
        mut bundle: serde_json::Value,
        passphrase: &str,
    ) -> Result<String, String> {
        validate_new_export_passphrase(passphrase)?;
        let object = bundle.as_object_mut().ok_or_else(|| "Export bundle must be a JSON object".to_string())?;
        if let Some(connections) = object.get_mut("connections").and_then(serde_json::Value::as_array_mut) {
            for connection in connections.iter_mut() {
                let input = ClientConnectionInput::from_value(connection.clone())?;
                let stored = self.load_connection(&input.config.id).await?;
                let merged = merge_stored_connection_secrets(&input.config, stored.as_ref(), &input.cleared_secrets)?;
                let merged = serde_json::to_value(&merged).map_err(|error| error.to_string())?;
                // Keep client-only fields (for example timeout inheritance
                // flags) while the resolved configuration wins elsewhere.
                let mut output = connection.as_object().cloned().unwrap_or_default();
                for field in ["saved_secrets", "cleared_secrets", "secrets_from_connection_id"] {
                    output.remove(field);
                }
                if let Some(merged) = merged.as_object() {
                    for (key, value) in merged {
                        output.insert(key.clone(), value.clone());
                    }
                }
                *connection = serde_json::Value::Object(output);
            }
        }
        if let Some(profiles) = object.get_mut("tunnelProfiles").and_then(serde_json::Value::as_array_mut) {
            let stored_profiles = self.load_tunnel_profiles().await?;
            for profile in profiles.iter_mut() {
                let Ok(mut parsed) = serde_json::from_value::<TransportLayerConfig>(profile.clone()) else {
                    continue;
                };
                if let Some(stored) = stored_profiles.iter().find(|stored| stored.id() == parsed.id()) {
                    fill_missing_tunnel_secrets(&mut parsed, stored);
                }
                *profile = serde_json::to_value(&parsed).map_err(|error| error.to_string())?;
            }
        }
        let plaintext = serde_json::to_string(&bundle).map_err(|error| error.to_string())?;
        let encrypted = encrypt_connection_export_blocking(plaintext, passphrase.to_string()).await?;
        serde_json::to_string_pretty(&encrypted).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231() {
        let mac = HmacSha256::new(b"Jefe").mac(&[b"what do ya want ", b"for nothing?"]);
        assert_eq!(hex(&mac), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn pbkdf2_sha256_matches_known_vectors() {
        assert_eq!(
            hex(&pbkdf2_sha256(b"password", b"salt", 1)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            hex(&pbkdf2_sha256(b"password", b"salt", 2)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        assert_eq!(
            hex(&pbkdf2_sha256(b"password", b"salt", 4096)),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    #[test]
    fn decrypts_version_1_files_written_by_the_browser() {
        let payload = serde_json::json!({
            "format": "dbx-encrypted",
            "version": 1,
            "salt": "AAECAwQFBgcICQoLDA0ODw==",
            "iv": "EBESExQVFhcYGRob",
            "data": "sCyBTex9XqcCCH5mOyJcF/UN9kpnMp+t0VeEtGrJBMt+QyR85kYhUWezuC9yEhM5jF0=",
        });
        assert_eq!(decrypt_connection_export(&payload, "passphrase").unwrap(), r#"{"connections":[{"name":"local"}]}"#);
        assert_eq!(decrypt_connection_export(&payload, "wrong").unwrap_err(), "wrong_passphrase");
    }

    #[test]
    fn version_2_roundtrips_and_authenticates_its_header() {
        let params = Argon2Params { memory_kib: 8 * 1024, iterations: 1, parallelism: 1 };
        let mut payload =
            encrypt_connection_export_with_params(r#"{"connections":[]}"#, "correct horse battery", params).unwrap();
        assert_eq!(payload["version"], 2);
        assert_eq!(payload["kdf"]["memoryKib"], 8 * 1024);
        assert_eq!(decrypt_connection_export(&payload, "correct horse battery").unwrap(), r#"{"connections":[]}"#);
        assert_eq!(decrypt_connection_export(&payload, "wrong horse battery").unwrap_err(), "wrong_passphrase");
        payload["kdf"]["iterations"] = serde_json::json!(2);
        assert_eq!(decrypt_connection_export(&payload, "correct horse battery").unwrap_err(), "wrong_passphrase");
    }

    #[test]
    fn new_exports_use_strong_parameters_and_require_long_passphrases() {
        assert!(encrypt_connection_export("{}", "short-pass").unwrap_err().starts_with("passphrase_too_short"));
        let payload = encrypt_connection_export("{}", "a sufficiently long passphrase").unwrap();
        assert_eq!(payload["kdf"]["memoryKib"], Argon2Params::for_new_data().memory_kib);
        const _: () = assert!(Argon2Params::STRONG.memory_kib >= 64 * 1024 && Argon2Params::STRONG.iterations >= 3);
        assert!(Argon2Params::STRONG.validate().is_ok());
    }

    #[test]
    fn rejects_unbounded_kdf_parameters_from_untrusted_files() {
        let mut payload = encrypt_connection_export_with_params(
            "{}",
            "correct horse battery",
            Argon2Params { memory_kib: 8 * 1024, iterations: 1, parallelism: 1 },
        )
        .unwrap();
        payload["kdf"]["memoryKib"] = serde_json::json!(u32::MAX);
        assert_eq!(
            decrypt_connection_export(&payload, "correct horse battery").unwrap_err(),
            "Unsupported key derivation parameters"
        );
    }

    #[tokio::test]
    async fn export_adds_stored_secrets_back_to_a_redacted_bundle() {
        use crate::connection_secrets::redact_connection_for_client;
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::storage::Storage::open(&dir.path().join("dbx.db")).await.unwrap();
        let mut config: crate::models::connection::ConnectionConfig = serde_json::from_value(serde_json::json!({
            "id": "prod", "name": "Prod", "db_type": "postgres", "host": "db", "port": 5432,
            "username": "app", "password": "db-secret", "database": null
        }))
        .unwrap();
        config.save_password = true;
        storage.save_connections(&[config.clone()]).await.unwrap();
        let mut redacted =
            redact_connection_for_client(&storage.load_connection("prod").await.unwrap().unwrap()).unwrap();
        redacted["connect_timeout_inherit"] = serde_json::json!(true);
        let bundle = serde_json::json!({ "connections": [redacted] });
        let file = storage.export_connections_encrypted(bundle, "a sufficiently long passphrase").await.unwrap();
        let payload: serde_json::Value = serde_json::from_str(&file).unwrap();
        let plaintext = decrypt_connection_export(&payload, "a sufficiently long passphrase").unwrap();
        let bundle: serde_json::Value = serde_json::from_str(&plaintext).unwrap();
        assert_eq!(bundle["connections"][0]["password"], "db-secret");
        assert_eq!(bundle["connections"][0]["connect_timeout_inherit"], true);
        assert!(bundle["connections"][0].get("saved_secrets").is_none());
    }
}
