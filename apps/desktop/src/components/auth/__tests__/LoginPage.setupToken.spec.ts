// @vitest-environment happy-dom

import { createApp, nextTick, type App } from "vue";
import { createI18n } from "vue-i18n";
import { afterEach, describe, expect, it, vi } from "vitest";
import LoginPage from "../LoginPage.vue";

const mountedApps: App[] = [];

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

async function flush() {
  for (let index = 0; index < 5; index += 1) {
    await Promise.resolve();
    await nextTick();
  }
}

async function mountSetup(fetchMock: ReturnType<typeof vi.fn>) {
  vi.stubGlobal("fetch", fetchMock);
  const container = document.createElement("div");
  document.body.append(container);
  const app = createApp(LoginPage, { setupMode: true });
  app.use(
    createI18n({
      legacy: false,
      locale: "en",
      missingWarn: false,
      fallbackWarn: false,
      messages: {
        en: {
          auth: {
            setupToken: "Setup token",
            setupTokenHint: "Find it in the server log",
            setupTokenInvalid: "Invalid setup token",
            loginFailed: "Incorrect password",
          },
        },
      },
    }),
  );
  mountedApps.push(app);
  app.mount(container);
  await flush();
  return container;
}

function typeInto(input: HTMLInputElement, value: string) {
  input.value = value;
  input.dispatchEvent(new Event("input"));
}

afterEach(() => {
  for (const app of mountedApps.splice(0)) app.unmount();
  document.body.innerHTML = "";
  vi.unstubAllGlobals();
});

describe("LoginPage setup token", () => {
  it("asks remote clients for the setup token and sends it with the new password", async () => {
    const fetchMock = vi.fn(async (url: string) => {
      if (String(url).endsWith("/api/auth/check")) return jsonResponse({ setup_required: true, setup_token_required: true });
      return jsonResponse({ error: "A valid setup token is required.", code: "SETUP_TOKEN_INVALID" }, 403);
    });
    const container = await mountSetup(fetchMock);

    const inputs = [...container.querySelectorAll("input")];
    expect(inputs).toHaveLength(3);
    expect(container.textContent).toContain("Find it in the server log");
    typeInto(inputs[0], " token-1 ");
    typeInto(inputs[1], "secret");
    typeInto(inputs[2], "secret");
    await nextTick();
    container.querySelector("form")?.dispatchEvent(new Event("submit"));
    await flush();

    const setupCall = fetchMock.mock.calls.find(([url]) => String(url).endsWith("/api/auth/setup"));
    expect(setupCall).toBeDefined();
    expect(JSON.parse(String((setupCall?.[1] as RequestInit).body))).toEqual({ password: "secret", setup_token: "token-1" });
    expect(container.textContent).toContain("Invalid setup token");
  });

  it("hides the token field for a browser on the server itself", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ setup_required: true, setup_token_required: false }));
    const container = await mountSetup(fetchMock);

    expect(container.querySelectorAll("input")).toHaveLength(2);
    expect(container.textContent).not.toContain("Find it in the server log");
  });
});
