import { describe, expect, it } from "vitest";
import { aiConfigIdForRequest, aiProxyUrlHasCredentials, effectiveAiClearedSecrets, hasAiApiKey, redactAiConfigSecrets } from "@/lib/ai/aiConfigSecrets";
import type { AiConfigItem } from "@/types/ai";

function item(overrides: Partial<AiConfigItem> = {}): AiConfigItem {
  return {
    id: "c1",
    name: "OpenAI",
    provider: "openai",
    apiKey: "",
    authMethod: "bearer",
    endpoint: "https://api.example.com/v1",
    model: "gpt",
    apiStyle: "completions",
    ...overrides,
  };
}

describe("hasAiApiKey", () => {
  it("treats a typed or backend-saved key as present", () => {
    expect(hasAiApiKey(item({ apiKey: "typed" }))).toBe(true);
    expect(hasAiApiKey(item({ savedSecrets: ["apiKey"] }))).toBe(true);
    expect(hasAiApiKey(item())).toBe(false);
    expect(hasAiApiKey(item({ savedSecrets: ["proxyUrl"] }))).toBe(false);
  });

  it("does not count a saved key the user cleared", () => {
    expect(hasAiApiKey(item({ savedSecrets: ["apiKey"], clearedSecrets: ["apiKey"] }))).toBe(false);
  });
});

describe("aiConfigIdForRequest", () => {
  it("prefers an explicit id and falls back to the config item id", () => {
    expect(aiConfigIdForRequest(item(), "explicit")).toBe("explicit");
    expect(aiConfigIdForRequest(item())).toBe("c1");
    expect(aiConfigIdForRequest({ provider: "openai" })).toBeUndefined();
    expect(aiConfigIdForRequest(item({ id: "  " }))).toBeUndefined();
    expect(aiConfigIdForRequest(null)).toBeUndefined();
  });
});

describe("aiProxyUrlHasCredentials", () => {
  it("detects userinfo in the proxy authority only", () => {
    expect(aiProxyUrlHasCredentials("http://user:pass@proxy.local:8080")).toBe(true);
    expect(aiProxyUrlHasCredentials("socks5://127.0.0.1:7890")).toBe(false);
    expect(aiProxyUrlHasCredentials("http://proxy.local/path@x")).toBe(false);
    expect(aiProxyUrlHasCredentials("")).toBe(false);
  });
});

describe("effectiveAiClearedSecrets", () => {
  it("keeps only stored secrets that are still blank", () => {
    const config = item({ apiKey: "replacement", proxyUrl: "" });
    expect(effectiveAiClearedSecrets(config, ["apiKey", "proxyUrl", "customHeaders.X-Unsaved"], ["apiKey", "proxyUrl"])).toEqual(["proxyUrl"]);
  });
});

describe("redactAiConfigSecrets", () => {
  it("blanks every secret value and records what the backend now stores", () => {
    const redacted = redactAiConfigSecrets(
      item({
        apiKey: "typed-key",
        proxyUrl: "http://user:pass@proxy.local:8080",
        customHeaders: { "X-Kept": "", "X-New": "value", "X-Empty": "" },
        codexCliEnv: { OPENAI_API_KEY: "sk-new", HTTPS_PROXY: "" },
        clearedSecrets: [],
      }),
      ["customHeaders.X-Kept", "codexCliEnv.HTTPS_PROXY"],
    );

    expect(redacted).toMatchObject({
      apiKey: "",
      proxyUrl: "",
      customHeaders: { "X-Kept": "", "X-New": "", "X-Empty": "" },
      codexCliEnv: { OPENAI_API_KEY: "", HTTPS_PROXY: "" },
    });
    expect(redacted.savedSecrets).toEqual(["apiKey", "proxyUrl", "customHeaders.X-Kept", "customHeaders.X-New", "codexCliEnv.OPENAI_API_KEY", "codexCliEnv.HTTPS_PROXY"]);
    expect(redacted.clearedSecrets).toBeUndefined();
  });

  it("keeps a credential-free proxy URL visible and honours explicit clears", () => {
    const redacted = redactAiConfigSecrets(item({ proxyUrl: "socks5://127.0.0.1:7890", clearedSecrets: ["apiKey"] }), ["apiKey", "proxyUrl"]);
    expect(redacted.proxyUrl).toBe("socks5://127.0.0.1:7890");
    expect(redacted.savedSecrets).toEqual([]);
  });
});
