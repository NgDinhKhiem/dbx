import { strict as assert } from "node:assert";
import { test } from "vitest";
import { isEncryptedConfig, MIN_EXPORT_PASSPHRASE_LENGTH, normalizeConfigCryptoError } from "../../apps/desktop/src/lib/backend/configCrypto.ts";

// Encryption and decryption run on the backend (see
// crates/dbx-core/src/persistence/connection_export.rs for the round-trip,
// v1 compatibility and passphrase-length tests). The frontend only detects the
// file format and maps backend error codes.

test("detects encrypted config formats", () => {
  assert.equal(isEncryptedConfig({ format: "dbx-encrypted", version: 1, salt: "a", iv: "b", data: "c" }), true);
  assert.equal(isEncryptedConfig({ format: "dbx-encrypted", version: 2, kdf: { name: "argon2id" }, cipher: "aes-256-gcm", nonce: "n", data: "c" }), true);
  assert.equal(isEncryptedConfig({ format: "dbx-encrypted", version: 2, kdf: null, nonce: "n", data: "c" }), false);
  assert.equal(isEncryptedConfig({ format: "dbx-encrypted", version: 3, data: "c" }), false);
  assert.equal(isEncryptedConfig({ format: "dbx-config", version: 1, connections: [] }), false);
  assert.equal(isEncryptedConfig([{ id: "1" }]), false);
  assert.equal(isEncryptedConfig(null), false);
  assert.equal(isEncryptedConfig("string"), false);
});

test("maps backend error codes to plain error messages", () => {
  assert.equal(normalizeConfigCryptoError("Error: wrong_passphrase").message, "wrong_passphrase");
  assert.equal(normalizeConfigCryptoError(new Error("request failed: passphrase_too_short")).message, "passphrase_too_short");
  assert.equal(normalizeConfigCryptoError({ message: "passphrase_required" }).message, "passphrase_required");
  const other = new Error("disk full");
  assert.equal(normalizeConfigCryptoError(other), other);
});

test("new exports require a long passphrase", () => {
  assert.ok(MIN_EXPORT_PASSPHRASE_LENGTH >= 12);
});
