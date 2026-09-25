/**
 * Legacy (version 1) encrypted config file: PBKDF2-SHA256 + AES-256-GCM.
 * Still accepted on import; new exports are produced by the backend as v2.
 */
export interface EncryptedPayloadV1 {
  format: "dbx-encrypted";
  version: 1;
  salt: string;
  iv: string;
  data: string;
}

/**
 * Version 2 encrypted config file produced by the backend
 * (`exportConnectionsEncrypted`): Argon2id key derivation + AES-256-GCM.
 */
export interface EncryptedPayloadV2 {
  format: "dbx-encrypted";
  version: 2;
  kdf: Record<string, unknown>;
  cipher: string;
  nonce: string;
  data: string;
}

export type EncryptedPayload = EncryptedPayloadV1 | EncryptedPayloadV2;

export interface PlainConfigPayload {
  format: "dbx-config";
  version: 1;
  connections: unknown[];
}

export const CONFIG_CRYPTO_UNAVAILABLE = "crypto_unavailable";
/** Minimum passphrase length accepted for new encrypted exports. */
export const MIN_EXPORT_PASSPHRASE_LENGTH = 12;

/**
 * Decrypts an encrypted config file. Decryption always happens on the backend
 * (Tauri command or web API) for both v1 and v2 files so the key derivation
 * parameters are enforced in one place. Rejects with `wrong_passphrase` when
 * the passphrase does not match.
 */
export async function decryptConfig(payload: EncryptedPayload, passphrase: string): Promise<string> {
  const { decryptConfig: decryptConfigOnBackend } = await import("@/lib/backend/api");
  try {
    return await decryptConfigOnBackend(payload, passphrase);
  } catch (error) {
    throw normalizeConfigCryptoError(error);
  }
}

/**
 * Backend errors arrive as strings (Tauri) or `Error`s with wrapped messages
 * (web). Map the well-known codes to `Error(code)` so callers can compare
 * `error.message` directly.
 */
export function normalizeConfigCryptoError(error: unknown): Error {
  const message = error instanceof Error ? error.message : typeof error === "string" ? error : String((error as { message?: unknown })?.message ?? error);
  for (const code of ["wrong_passphrase", "passphrase_too_short", "passphrase_required"]) {
    if (message.includes(code)) return new Error(code);
  }
  return error instanceof Error ? error : new Error(message);
}

export function isEncryptedConfig(data: unknown): data is EncryptedPayload {
  if (typeof data !== "object" || data === null) return false;
  const obj = data as Record<string, unknown>;
  if (obj.format !== "dbx-encrypted" || typeof obj.data !== "string") return false;
  if (obj.version === 1) return typeof obj.salt === "string" && typeof obj.iv === "string";
  if (obj.version === 2) return typeof obj.nonce === "string" && typeof obj.kdf === "object" && obj.kdf !== null;
  return false;
}
