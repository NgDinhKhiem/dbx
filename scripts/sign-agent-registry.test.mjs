import assert from "node:assert/strict";
import { createPublicKey, generateKeyPairSync, verify } from "node:crypto";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { parseSigningKey, publicKeyBase64, SIGNING_KEY_ENV } from "./sign-agent-registry.mjs";

const script = new URL("./sign-agent-registry.mjs", import.meta.url).pathname;

function runSign(args, key) {
  return spawnSync(process.execPath, [script, ...args], {
    encoding: "utf8",
    env: { ...process.env, [SIGNING_KEY_ENV]: key },
  });
}

function ed25519PublicKeyFromBase64(value) {
  const prefix = Buffer.from("302a300506032b6570032100", "hex");
  return createPublicKey({ key: Buffer.concat([prefix, Buffer.from(value, "base64")]), format: "der", type: "spki" });
}

test("signs the exact registry bytes with a PEM key and writes <path>.sig", () => {
  const { privateKey } = generateKeyPairSync("ed25519");
  const pem = privateKey.export({ format: "pem", type: "pkcs8" }).toString();
  const directory = mkdtempSync(join(tmpdir(), "dbx-sign-registry-"));
  const registryPath = join(directory, "agent-registry.json");
  const registry = Buffer.from('{\n  "jres": {},\n  "drivers": {}\n}\n');
  writeFileSync(registryPath, registry);

  const result = runSign([registryPath], pem.replace(/\n/g, "\\n"));
  assert.equal(result.status, 0, result.stderr);

  const signature = Buffer.from(readFileSync(`${registryPath}.sig`, "utf8").trim(), "base64");
  assert.equal(signature.length, 64);
  const printed = runSign(["--print-public-key"], pem);
  assert.equal(printed.status, 0, printed.stderr);
  const publicKey = ed25519PublicKeyFromBase64(printed.stdout.trim());
  assert.ok(verify(null, registry, publicKey, signature));
  assert.ok(!verify(null, Buffer.from('{"jres":{},"drivers":{}}'), publicKey, signature));
});

test("accepts a base64 raw seed and rejects malformed keys", () => {
  const seed = Buffer.alloc(32, 7).toString("base64");
  const key = parseSigningKey(seed);
  assert.equal(key.asymmetricKeyType, "ed25519");
  assert.equal(Buffer.from(publicKeyBase64(key), "base64").length, 32);
  assert.throws(() => parseSigningKey(""), /is not set/);
  assert.throws(() => parseSigningKey(Buffer.alloc(31).toString("base64")), /32-byte/);
});

test("refuses to sign a file that is not JSON", () => {
  const directory = mkdtempSync(join(tmpdir(), "dbx-sign-registry-invalid-"));
  const registryPath = join(directory, "agent-registry.json");
  writeFileSync(registryPath, "not json");
  const result = runSign([registryPath], Buffer.alloc(32, 1).toString("base64"));
  assert.notEqual(result.status, 0);
});
