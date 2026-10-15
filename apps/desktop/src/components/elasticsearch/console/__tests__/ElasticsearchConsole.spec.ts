// @vitest-environment happy-dom
import { createApp, defineComponent, h, nextTick, type App } from "vue";
import { createPinia, setActivePinia } from "pinia";
import { createI18n } from "vue-i18n";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import en from "@/i18n/locales/en";

const mocks = vi.hoisted(() => ({
  sendConsoleRequest: vi.fn(),
  connection: { id: "es-1", name: "Local ES", db_type: "elasticsearch", host: "localhost", port: 9202 } as Record<string, unknown>,
  settings: { editorSettings: { theme: "app", fontSize: 13, fontFamily: "monospace", wordWrap: false, confirmDangerousSqlExecution: true } },
}));

vi.mock("@/lib/elasticsearch/console/consoleApi", () => ({
  sendConsoleRequest: mocks.sendConsoleRequest,
}));

vi.mock("@/stores/connectionStore", () => ({
  useConnectionStore: () => ({
    getConfig: () => mocks.connection,
    listElasticsearchCompletionIndices: vi.fn(async () => ["logs"]),
    listElasticsearchCompletionFields: vi.fn(async () => []),
  }),
}));

vi.mock("@/stores/settingsStore", () => ({
  useSettingsStore: () => mocks.settings,
}));

vi.mock("@/composables/useTheme", async () => {
  const { ref: vueRef } = await import("vue");
  return { useTheme: () => ({ isDark: vueRef(true), themePalette: vueRef("pearl") }) };
});

vi.mock("@/components/editor/DangerConfirmDialog.vue", async () => {
  const { defineComponent: define, h: render } = await import("vue");
  return {
    default: define({
      name: "DangerConfirmDialogStub",
      props: { open: Boolean, sql: String, message: String, confirmLabel: String },
      emits: ["confirm", "update:open"],
      setup(props, { emit }) {
        return () =>
          render("div", { "data-testid": "danger-dialog" }, [
            render("pre", { "data-testid": "danger-sql" }, props.sql),
            render("button", { "data-testid": "danger-confirm", onClick: () => emit("confirm") }, "confirm"),
            render("button", { "data-testid": "danger-cancel", onClick: () => emit("update:open", false) }, "cancel"),
          ]);
      },
    }),
  };
});

import ElasticsearchConsole from "../ElasticsearchConsole.vue";

let app: App | null = null;
let root: HTMLElement | null = null;

async function flush(times = 5) {
  for (let index = 0; index < times; index += 1) {
    await Promise.resolve();
    await nextTick();
  }
}

async function waitFor<T>(read: () => T | null | undefined | false, attempts = 200): Promise<T> {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    const value = read();
    if (value) return value;
    await new Promise((resolve) => setTimeout(resolve, 5));
    await nextTick();
  }
  throw new Error("waitFor timed out");
}

function editorViews(): EditorView[] {
  return [...document.querySelectorAll<HTMLElement>(".cm-editor")].map((element) => EditorView.findFromDOM(element)).filter((view): view is EditorView => view !== null);
}

async function mountConsole(initialText: string) {
  const textChanges: string[] = [];
  root = document.createElement("div");
  root.style.height = "600px";
  document.body.appendChild(root);
  const i18n = createI18n({ legacy: false, locale: "en", messages: { en } });
  const Host = defineComponent({
    setup() {
      return () => h(ElasticsearchConsole, { connectionId: "es-1", initialText, onTextChange: (value: string) => textChanges.push(value) });
    },
  });
  app = createApp(Host);
  app.use(createPinia());
  app.use(i18n);
  app.mount(root);
  const views = await waitFor(() => {
    const found = editorViews();
    return found.length >= 2 ? found : null;
  });
  const requestView = views.find((view) => view.contentDOM.dataset.testid === "es-console-request-editor");
  const responseView = views.find((view) => view.contentDOM.dataset.testid === "es-console-response-editor");
  if (!requestView || !responseView) throw new Error("editors not mounted");
  return { requestView, responseView, textChanges };
}

function pressModEnter(view: EditorView) {
  view.contentDOM.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", metaKey: true, ctrlKey: false, bubbles: true, cancelable: true }));
}

function placeCursor(view: EditorView, needle: string) {
  const position = view.state.doc.toString().indexOf(needle);
  if (position < 0) throw new Error(`missing ${needle}`);
  view.dispatch({ selection: { anchor: position } });
}

beforeEach(() => {
  setActivePinia(createPinia());
  mocks.sendConsoleRequest.mockReset();
  mocks.connection = { id: "es-1", name: "Local ES", db_type: "elasticsearch", host: "localhost", port: 9202 };
  mocks.settings.editorSettings.confirmDangerousSqlExecution = true;
  const store = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => void store.set(key, String(value)),
    removeItem: (key: string) => void store.delete(key),
    clear: () => store.clear(),
  });
});

afterEach(() => {
  app?.unmount();
  app = null;
  root?.remove();
  root = null;
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("ElasticsearchConsole", () => {
  it("runs the request under the cursor on Cmd+Enter and renders status and body", async () => {
    mocks.sendConsoleRequest.mockResolvedValue({ status: 200, body: '{"acknowledged":true,"n":12345678901234567890}', tookMs: 17 });
    const { requestView, responseView } = await mountConsole('GET _cat/indices?v\n\nGET _search\n{"query":{"match_all":{}}}\n');

    placeCursor(requestView, "match_all");
    pressModEnter(requestView);
    await waitFor(() => document.querySelector('[data-testid="es-console-status"]'));

    expect(mocks.sendConsoleRequest).toHaveBeenCalledTimes(1);
    expect(mocks.sendConsoleRequest).toHaveBeenCalledWith("es-1", { method: "GET", path: "/_search", body: '{"query":{"match_all":{}}}' });
    expect(document.querySelector('[data-testid="es-console-status"]')?.textContent).toBe("200 - OK");
    expect(document.querySelector('[data-testid="es-console-took"]')?.textContent).toBe("17 ms");
    await waitFor(() => responseView.state.doc.length > 0);
    expect(responseView.state.doc.toString()).toBe('{\n  "acknowledged": true,\n  "n": 12345678901234567890\n}');

    const history = JSON.parse(localStorage.getItem("dbx.esConsole.history.v1.es-1") ?? "[]");
    expect(history.map((entry: { label: string }) => entry.label)).toEqual(["GET _search"]);
    expect(JSON.stringify(history)).not.toContain("acknowledged");
  });

  it("shows plain-text responses and client errors with their status", async () => {
    mocks.sendConsoleRequest.mockResolvedValue({ status: 404, body: "not found\n", tookMs: 2 });
    const { requestView, responseView } = await mountConsole("GET nope/_cat\n");
    placeCursor(requestView, "nope");
    pressModEnter(requestView);
    await waitFor(() => responseView.state.doc.length > 0);
    expect(responseView.state.doc.toString()).toBe("not found\n");
    expect(document.querySelector('[data-testid="es-console-status"]')?.textContent).toBe("404 - Not Found");
  });

  it("runs every selected request sequentially and separates the responses", async () => {
    mocks.sendConsoleRequest.mockResolvedValueOnce({ status: 200, body: "green\n", tookMs: 1 }).mockResolvedValueOnce({ status: 200, body: '{"count":3}', tookMs: 2 });
    const { requestView, responseView } = await mountConsole("GET _cat/health\n\nGET logs/_count\n");
    requestView.dispatch({ selection: { anchor: 0, head: requestView.state.doc.length } });
    pressModEnter(requestView);
    await waitFor(() => responseView.state.doc.toString().includes("count"));
    expect(mocks.sendConsoleRequest.mock.calls.map((call) => call[1].path)).toEqual(["/_cat/health", "/logs/_count"]);
    expect(responseView.state.doc.toString()).toBe('# GET _cat/health  200 OK\ngreen\n\n# GET logs/_count  200 OK\n{\n  "count": 3\n}\n');
  });

  it("asks for confirmation before sending a write request", async () => {
    mocks.sendConsoleRequest.mockResolvedValue({ status: 200, body: '{"acknowledged":true}', tookMs: 5 });
    const { requestView } = await mountConsole("GET _search\n\nDELETE logs\n");

    placeCursor(requestView, "DELETE");
    pressModEnter(requestView);
    const dialog = await waitFor(() => document.querySelector('[data-testid="danger-dialog"]'));
    expect(dialog.querySelector('[data-testid="danger-sql"]')?.textContent).toBe("DELETE logs");
    await flush();
    expect(mocks.sendConsoleRequest).not.toHaveBeenCalled();

    (document.querySelector('[data-testid="danger-confirm"]') as HTMLButtonElement).click();
    await waitFor(() => mocks.sendConsoleRequest.mock.calls.length > 0);
    expect(mocks.sendConsoleRequest).toHaveBeenCalledWith("es-1", { method: "DELETE", path: "/logs", body: undefined });
    await flush();
    expect(document.querySelector('[data-testid="danger-dialog"]')).toBeNull();
  });

  it("does not send a write request when the confirmation is cancelled", async () => {
    const { requestView } = await mountConsole('POST logs/_delete_by_query\n{"query":{"match_all":{}}}\n');
    placeCursor(requestView, "POST");
    pressModEnter(requestView);
    await waitFor(() => document.querySelector('[data-testid="danger-dialog"]'));
    (document.querySelector('[data-testid="danger-cancel"]') as HTMLButtonElement).click();
    await flush();
    expect(document.querySelector('[data-testid="danger-dialog"]')).toBeNull();
    expect(mocks.sendConsoleRequest).not.toHaveBeenCalled();
  });

  it("shows read-only rejections clearly", async () => {
    mocks.settings.editorSettings.confirmDangerousSqlExecution = false;
    mocks.sendConsoleRequest.mockRejectedValue(new Error("READ_ONLY: connection 'Local ES' has read-only protection enabled."));
    const { requestView, responseView } = await mountConsole("PUT logs/_doc/1\n{}\n");
    placeCursor(requestView, "PUT");
    pressModEnter(requestView);
    await waitFor(() => document.querySelector('[data-testid="es-console-read-only"]'));
    expect(document.querySelector('[data-testid="es-console-status"]')?.textContent).toBe("Request failed");
    expect(responseView.state.doc.toString()).toContain("READ_ONLY");
  });

  it("emits debounced text changes", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { requestView, textChanges } = await mountConsole("GET _search\n");
    requestView.dispatch({ changes: { from: requestView.state.doc.length, insert: "GET _cat/nodes\n" } });
    requestView.dispatch({ changes: { from: requestView.state.doc.length, insert: "# end\n" } });
    expect(textChanges).toEqual([]);
    await vi.advanceTimersByTimeAsync(450);
    expect(textChanges).toEqual(["GET _search\nGET _cat/nodes\n# end\n"]);
  });
});
