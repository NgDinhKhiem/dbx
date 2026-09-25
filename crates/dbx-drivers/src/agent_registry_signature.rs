//! Detached Ed25519 signatures for the online `agent-registry.json`.
//!
//! The registry pins the URL, size and SHA-256 of every agent artifact (driver
//! JARs, native binaries/bundles and JREs), so authenticating the registry
//! authenticates everything installed from it. The registry is signed at
//! release time by `scripts/sign-agent-registry.mjs`, which writes
//! `agent-registry.json.sig` next to it: the standard base64 encoding of a
//! 64-byte Ed25519 signature over the exact registry file bytes. DBX fetches
//! the `.sig` from the same mirror as the registry and verifies it before
//! parsing.
//!
//! # Where to put the key
//!
//! Add the release signing public key to [`PINNED_AGENT_REGISTRY_PUBLIC_KEYS`]
//! as the standard base64 encoding of the raw 32-byte Ed25519 public key.
//! `AGENT_REGISTRY_SIGNING_KEY=... node scripts/sign-agent-registry.mjs --print-public-key`
//! prints it in that format. Keep the previous key in the list during a
//! rotation until every published registry is signed with the new one.
//!
//! While the list is empty (and `DBX_AGENT_REGISTRY_PUBKEYS` is unset), the
//! registry is accepted unsigned with a warning, and integrity relies on the
//! mandatory per-artifact SHA-256. As soon as at least one key is configured,
//! a valid signature is required and an unsigned or mis-signed registry is
//! rejected (the next mirror is tried).
//!
//! `DBX_AGENT_REGISTRY_PUBKEYS` adds keys (comma or whitespace separated, same
//! encoding) for testing a signed registry before the key is pinned here.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};

/// Pinned release keys: standard base64 of the raw 32-byte Ed25519 public key.
/// Empty until the project owner provides the release signing key.
pub const PINNED_AGENT_REGISTRY_PUBLIC_KEYS: &[&str] = &[];

/// Extra trusted keys for testing (comma or whitespace separated base64).
pub const AGENT_REGISTRY_PUBKEYS_ENV: &str = "DBX_AGENT_REGISTRY_PUBKEYS";

/// Suffix appended to the registry URL to locate its detached signature.
pub const AGENT_REGISTRY_SIGNATURE_SUFFIX: &str = ".sig";

/// A base64 signature is 88 characters; allow whitespace and a trailing newline.
pub const MAX_AGENT_REGISTRY_SIGNATURE_BYTES: usize = 1024;

/// Pinned keys plus any keys from [`AGENT_REGISTRY_PUBKEYS_ENV`]. An invalid
/// configured key is an error (fail closed) rather than being skipped.
pub fn trusted_registry_keys() -> Result<Vec<VerifyingKey>, String> {
    let env_value = std::env::var(AGENT_REGISTRY_PUBKEYS_ENV).unwrap_or_default();
    trusted_registry_keys_from(PINNED_AGENT_REGISTRY_PUBLIC_KEYS, &env_value)
}

fn trusted_registry_keys_from(pinned: &[&str], env_value: &str) -> Result<Vec<VerifyingKey>, String> {
    let env_keys = env_value.split(|ch: char| ch == ',' || ch.is_whitespace()).filter(|key| !key.is_empty());
    pinned
        .iter()
        .copied()
        .chain(env_keys)
        .map(|encoded| {
            parse_public_key(encoded).map_err(|error| format!("Invalid agent registry public key {encoded:?}: {error}"))
        })
        .collect()
}

fn parse_public_key(encoded: &str) -> Result<VerifyingKey, String> {
    let bytes = BASE64.decode(encoded.trim()).map_err(|error| format!("not base64: {error}"))?;
    let bytes: [u8; 32] =
        bytes.try_into().map_err(|bytes: Vec<u8>| format!("expected 32 bytes, got {}", bytes.len()))?;
    VerifyingKey::from_bytes(&bytes).map_err(|error| error.to_string())
}

/// Verifies `signature_text` (base64, surrounding whitespace ignored) over the
/// exact `registry` bytes against any of `keys`.
pub fn verify_registry_signature(registry: &[u8], signature_text: &str, keys: &[VerifyingKey]) -> Result<(), String> {
    let signature_bytes = BASE64
        .decode(signature_text.trim())
        .map_err(|error| format!("Agent registry signature is not valid base64: {error}"))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|error| format!("Agent registry signature is malformed: {error}"))?;
    if keys.iter().any(|key| key.verify_strict(registry, &signature).is_ok()) {
        Ok(())
    } else {
        Err("Agent registry signature does not match any trusted key".to_string())
    }
}

/// Logs (once per process) that the registry is accepted without a signature.
pub fn warn_unsigned_registry_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        log::warn!(
            "[agents] no agent registry signing key is configured; accepting agent-registry.json without a \
             signature and relying on the mandatory per-artifact SHA-256 (see agent_registry_signature.rs)"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn public_key_b64(key: &SigningKey) -> String {
        BASE64.encode(key.verifying_key().to_bytes())
    }

    #[test]
    fn pinned_key_list_is_empty_until_the_release_key_is_provided() {
        assert!(trusted_registry_keys_from(PINNED_AGENT_REGISTRY_PUBLIC_KEYS, "").unwrap().is_empty());
    }

    #[test]
    fn env_keys_extend_the_pinned_keys_and_invalid_keys_fail_closed() {
        let first = public_key_b64(&signing_key(1));
        let second = public_key_b64(&signing_key(2));
        let keys = trusted_registry_keys_from(&[first.as_str()], &format!(" {second},\n")).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(trusted_registry_keys_from(&[], "not-a-key")
            .unwrap_err()
            .contains("Invalid agent registry public key"));
        assert!(trusted_registry_keys_from(&[], &BASE64.encode([0_u8; 31])).unwrap_err().contains("32 bytes"));
    }

    #[test]
    fn signature_must_cover_the_exact_registry_bytes_and_a_trusted_key() {
        let key = signing_key(7);
        let registry = br#"{"drivers":{}}"#;
        let signature = format!("{}\n", BASE64.encode(key.sign(registry).to_bytes()));
        let trusted = trusted_registry_keys_from(&[], &public_key_b64(&key)).unwrap();

        verify_registry_signature(registry, &signature, &trusted).unwrap();
        assert!(verify_registry_signature(br#"{"drivers": {}}"#, &signature, &trusted)
            .unwrap_err()
            .contains("does not match"));
        let other = trusted_registry_keys_from(&[], &public_key_b64(&signing_key(8))).unwrap();
        assert!(verify_registry_signature(registry, &signature, &other).is_err());
        assert!(verify_registry_signature(registry, "%%%", &trusted).unwrap_err().contains("base64"));
        assert!(verify_registry_signature(registry, &BASE64.encode([1_u8; 10]), &trusted)
            .unwrap_err()
            .contains("malformed"));
    }
}
