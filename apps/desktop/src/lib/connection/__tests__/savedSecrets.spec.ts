import { describe, expect, it } from "vitest";
import type { ConnectionConfig } from "@/types/database";
import { clearSavedSecret, connectionSecretPath, hasSavedSecret, presentSecretPaths, redactSavedConnection, redactUrlParams, secretAvailable, stripSecretMetadata, transportLayerSecretPath, urlParamsHaveSensitiveValue } from "../savedSecrets";

function conn(extras: Partial<ConnectionConfig> = {}): ConnectionConfig {
  return { id: "c1", name: "C1", db_type: "postgres", host: "db", port: 5432, username: "app", password: "", ...extras };
}

describe("savedSecrets", () => {
  it("reports saved secrets unless they were cleared", () => {
    const config = conn({ saved_secrets: ["password", "init_script"], cleared_secrets: ["init_script"] });
    expect(hasSavedSecret(config, "password")).toBe(true);
    expect(hasSavedSecret(config, "init_script")).toBe(false);
    expect(hasSavedSecret(config, "connection_string")).toBe(false);
    expect(hasSavedSecret(undefined, "password")).toBe(false);
    expect(secretAvailable(config, "password", "")).toBe(true);
    expect(secretAvailable(conn(), "password", "")).toBe(false);
    expect(secretAvailable(conn(), "password", "typed")).toBe(true);
  });

  it("builds transport layer paths from the layer id or its index", () => {
    expect(transportLayerSecretPath({ id: "ssh-1" }, 0, "password")).toBe("transport_layers.ssh-1.password");
    expect(transportLayerSecretPath({ id: "" }, 2, "key_passphrase")).toBe("transport_layers.#2.key_passphrase");
    expect(connectionSecretPath("api_token")).toBe("connection_secrets.api_token");
  });

  it("clears a saved secret once", () => {
    const config = conn({ saved_secrets: ["password", "init_script"] });
    clearSavedSecret(config, "password");
    clearSavedSecret(config, "password");
    expect(config.saved_secrets).toEqual(["init_script"]);
    expect(config.cleared_secrets).toEqual(["password"]);
    expect(hasSavedSecret(config, "password")).toBe(false);
  });

  it("detects and blanks sensitive URL param values with either separator", () => {
    expect(urlParamsHaveSensitiveValue("sslmode=require&password=x")).toBe(true);
    expect(urlParamsHaveSensitiveValue("sslmode=require;Client_Secret=x")).toBe(true);
    expect(urlParamsHaveSensitiveValue("sslmode=require&password=")).toBe(false);
    expect(urlParamsHaveSensitiveValue(undefined)).toBe(false);
    expect(redactUrlParams("sslmode=require&PASSWORD=x;api-key=y;flag")).toBe("sslmode=require&PASSWORD=;api-key=;flag");
    expect(redactUrlParams(undefined)).toBeUndefined();
  });

  it("lists typed secrets for every supported location", () => {
    const paths = presentSecretPaths(
      conn({
        password: "pw",
        init_script: "SET x",
        url_params: "token=abc",
        connection_secrets: { api_token: "t", empty: "" },
        transport_layers: [
          { type: "ssh", id: "s", host: "h", port: 22, user: "u", password: "sp", key_passphrase: "" },
          { type: "proxy", id: "", host: "p", port: 1080, password: "pp" },
          { type: "http_tunnel", id: "t", url: "https://t", token: "tok" },
        ],
      }),
    );
    expect(paths).toEqual(["password", "init_script", "url_params", "transport_layers.s.password", "transport_layers.#1.password", "transport_layers.t.token", "connection_secrets.api_token"]);
  });

  it("only treats external_config fields as secrets for the owning connection type", () => {
    const external = { auth: { kind: "apiKey", header: "X-Key", value: "k", password: "p" }, tokenSigning: { algorithm: "hs256", key: "sig" } };
    expect(presentSecretPaths(conn({ db_type: "mq", external_config: external }))).toEqual(["external_config.auth.password", "external_config.auth.value", "external_config.tokenSigning.key"]);
    expect(presentSecretPaths(conn({ db_type: "mq", external_config: { auth: { kind: "basic", value: "public" } } }))).toEqual([]);
    expect(presentSecretPaths(conn({ db_type: "postgres", external_config: external }))).toEqual([]);
    expect(presentSecretPaths(conn({ db_type: "cassandra", external_config: { tls: { truststore_path: "/t", truststore_password: "a", keystore_password: "" } } }))).toEqual(["external_config.tls.truststore_password"]);
  });

  it("redacts a persisted connection and recomputes saved_secrets", () => {
    const source = conn({
      password: "pw",
      saved_secrets: ["init_script", "transport_layers.gone.password", "connection_string"],
      cleared_secrets: ["connection_string"],
      secrets_from_connection_id: "other",
      url_params: "sslmode=require&password=x",
      redis_sentinel_password: "rs",
      connection_secrets: { api_token: "t" },
      transport_layers: [{ type: "ssh", id: "s", host: "h", port: 22, user: "u", password: "sp", key_passphrase: "kp" }],
      db_type: "nacos",
      external_config: { serverAddr: "n:8848", auth: { kind: "usernamePassword", username: "nacos", password: "np" } },
    });
    const redacted = redactSavedConnection(source);
    expect(redacted.password).toBe("");
    expect(redacted.redis_sentinel_password).toBe("");
    expect(redacted.url_params).toBe("sslmode=require&password=");
    expect(redacted.connection_secrets).toEqual({});
    expect(redacted.transport_layers?.[0]).toMatchObject({ password: "", key_passphrase: "", user: "u" });
    expect(redacted.external_config).toEqual({ serverAddr: "n:8848", auth: { kind: "usernamePassword", username: "nacos", password: "" } });
    expect(redacted.cleared_secrets).toBeUndefined();
    expect(redacted.secrets_from_connection_id).toBeUndefined();
    expect([...(redacted.saved_secrets ?? [])].sort()).toEqual(["connection_secrets.api_token", "external_config.auth.password", "init_script", "password", "redis_sentinel_password", "transport_layers.s.key_passphrase", "transport_layers.s.password", "url_params"]);
    expect(JSON.stringify(redacted)).not.toMatch(/"(pw|rs|sp|kp|np|t)"/);
    // the source object is not mutated
    expect(source.password).toBe("pw");
  });

  it("never marks the password saved when saving passwords is disabled", () => {
    const redacted = redactSavedConnection(conn({ password: "pw", save_password: false, saved_secrets: ["password"] }));
    expect(redacted.saved_secrets).toEqual([]);
    expect(redacted.password).toBe("");
  });

  it("strips secret bookkeeping fields", () => {
    expect(stripSecretMetadata(conn({ saved_secrets: ["password"], cleared_secrets: [], secrets_from_connection_id: "x" }))).toEqual(conn());
  });
});
