#!/usr/bin/env node
// Signs agent-registry.json with a detached Ed25519 signature.
//
// Usage:
//   AGENT_REGISTRY_SIGNING_KEY=... node scripts/sign-agent-registry.mjs release/agent-registry.json
//     Writes release/agent-registry.json.sig: the standard base64 encoding of
//     the 64-byte Ed25519 signature over the exact file bytes, plus a newline.
//     The signature is verified against the derived public key before writing.
//
//   AGENT_REGISTRY_SIGNING_KEY=... node scripts/sign-agent-registry.mjs --print-public-key
//     Prints the public key as standard base64 of the raw 32 bytes. That value
//     goes into PINNED_AGENT_REGISTRY_PUBLIC_KEYS in
//     crates/dbx-drivers/src/agent_registry_signature.rs (or, for testing, into
//     the DBX_AGENT_REGISTRY_PUBKEYS environment variable of the app).
//
// Accepted formats for AGENT_REGISTRY_SIGNING_KEY (the private key):
//   * PEM PKCS#8 ("-----BEGIN PRIVATE KEY-----"), e.g. from
//       openssl genpkey -algorithm ed25519 -out agent-registry-signing.pem
//     Literal "\n" sequences are accepted in place of newlines (CI secrets).
//   * Standard base64 of the raw 32-byte Ed25519 seed, e.g. from
//       node -e "console.log(require('crypto').randomBytes(32).toString('base64'))"
//
// Never commit the private key; store it only as a CI secret.

import { createPrivateKey, createPublicKey, sign, verify } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

export const SIGNING_KEY_ENV = "AGENT_REGISTRY_SIGNING_KEY";
export const SIGNATURE_SUFFIX = ".sig";

// DER prefix of a PKCS#8 Ed25519 private key; the 32-byte seed follows it.
const ED25519_PKCS8_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex");
// DER prefix of an SPKI Ed25519 public key; the 32-byte key follows it.
const ED25519_SPKI_PREFIX = Buffer.from("302a300506032b6570032100", "hex");

export function parseSigningKey(value) {
  const text = (value ?? "").trim();
  if (!text) {
    throw new Error(`${SIGNING_KEY_ENV} is not set`);
  }
  let key;
  if (text.includes("-----BEGIN")) {
    key = createPrivateKey({ key: text.replace(/\\n/g, "\n"), format: "pem" });
  } else {
    const seed = Buffer.from(text, "base64");
    if (seed.length !== 32 || seed.toString("base64").replace(/=+$/, "") !== text.replace(/=+$/, "")) {
      throw new Error(`${SIGNING_KEY_ENV} must be a PEM PKCS#8 key or base64 of a 32-byte Ed25519 seed`);
    }
    key = createPrivateKey({ key: Buffer.concat([ED25519_PKCS8_PREFIX, seed]), format: "der", type: "pkcs8" });
  }
  if (key.asymmetricKeyType !== "ed25519") {
    throw new Error(`${SIGNING_KEY_ENV} must be an Ed25519 key, got ${key.asymmetricKeyType}`);
  }
  return key;
}

export function publicKeyBase64(privateKey) {
  const spki = createPublicKey(privateKey).export({ format: "der", type: "spki" });
  if (spki.length !== ED25519_SPKI_PREFIX.length + 32 || !spki.subarray(0, ED25519_SPKI_PREFIX.length).equals(ED25519_SPKI_PREFIX)) {
    throw new Error("Unexpected Ed25519 public key encoding");
  }
  return spki.subarray(ED25519_SPKI_PREFIX.length).toString("base64");
}

export function signRegistryBytes(bytes, privateKey) {
  const signature = sign(null, bytes, privateKey);
  if (!verify(null, bytes, createPublicKey(privateKey), signature)) {
    throw new Error("Self-verification of the agent registry signature failed");
  }
  return signature.toString("base64");
}

export function signRegistryFile(registryPath, privateKey) {
  const bytes = readFileSync(registryPath);
  JSON.parse(bytes.toString("utf8"));
  const signaturePath = `${registryPath}${SIGNATURE_SUFFIX}`;
  writeFileSync(signaturePath, `${signRegistryBytes(bytes, privateKey)}\n`);
  return signaturePath;
}

function main(args) {
  const privateKey = parseSigningKey(process.env[SIGNING_KEY_ENV]);
  if (args.length === 1 && args[0] === "--print-public-key") {
    console.log(publicKeyBase64(privateKey));
    return;
  }
  if (args.length !== 1 || args[0].startsWith("-")) {
    throw new Error("Usage: node scripts/sign-agent-registry.mjs <agent-registry.json> | --print-public-key");
  }
  const signaturePath = signRegistryFile(resolve(args[0]), privateKey);
  console.log(`Signed ${args[0]} -> ${signaturePath} (public key ${publicKeyBase64(privateKey)})`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(`sign-agent-registry: ${error.message}`);
    process.exit(1);
  }
}
