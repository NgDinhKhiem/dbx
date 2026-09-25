// @vitest-environment happy-dom

import { createApp, nextTick, ref, type App } from "vue";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { InstalledPlugin, PluginWorkbenchContribution } from "@/types/database";

const mocks = vi.hoisted(() => ({
  readPluginUiEntry: vi.fn(),
  readPluginUiAsset: vi.fn(),
  subscribePluginEvents: vi.fn(),
  repushPluginConnection: vi.fn(),
  reopenPluginConnection: vi.fn(),
  openPluginLocalFile: vi.fn(),
  readPluginLocalFileChunk: vi.fn(),
  writePluginLocalFileChunk: vi.fn(),
  closePluginLocalFile: vi.fn(),
}));

vi.mock("@/lib/backend/tauri", () => ({
  openPluginLocalFile: mocks.openPluginLocalFile,
  readPluginLocalFileChunk: mocks.readPluginLocalFileChunk,
  writePluginLocalFileChunk: mocks.writePluginLocalFileChunk,
  closePluginLocalFile: mocks.closePluginLocalFile,
}));

vi.mock("@/lib/backend/api", () => ({
  invokePlugin: vi.fn(),
  notifyPlugin: vi.fn(),
  sendPluginBinary: vi.fn(),
  readPluginUiEntry: mocks.readPluginUiEntry,
  readPluginUiAsset: mocks.readPluginUiAsset,
  subscribePluginEvents: mocks.subscribePluginEvents,
}));
vi.mock("@/lib/backend/tauriRuntime", () => ({ isTauriRuntime: () => false }));
vi.mock("@/lib/common/clipboard", () => ({ copyToClipboard: vi.fn() }));
vi.mock("@/composables/useTheme", () => ({ useTheme: () => ({ isDark: ref(false), themeRevision: ref(0) }) }));
vi.mock("@/stores/settingsStore", () => ({ useSettingsStore: () => ({ editorSettings: { uiFontFamily: "", fontFamily: "", fontSize: 14 } }) }));
vi.mock("@/stores/connectionStore", () => ({
  useConnectionStore: () => ({
    repushPluginConnection: mocks.repushPluginConnection,
    reopenPluginConnection: mocks.reopenPluginConnection,
  }),
}));
vi.mock("vue-i18n", () => ({ useI18n: () => ({ locale: ref("en"), t: (key: string) => key }) }));

import PluginWorkbenchHost from "./PluginWorkbenchHost.vue";

const plugin: InstalledPlugin = {
  manifest: { id: "sample", name: "Sample", version: "1.0.0", permissions: [], drivers: [], contributions: [] },
  compatibility: { compatible: true },
};
const contribution: PluginWorkbenchContribution = { type: "workbench", id: "sample.main", label: "Sample" };

async function flushWorkbenchLoad() {
  await nextTick();
  await new Promise((resolve) => setTimeout(resolve, 0));
  await nextTick();
}

describe("PluginWorkbenchHost initialization", () => {
  let app: App<Element> | undefined;
  let root: HTMLDivElement;

  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => window.setTimeout(() => callback(0), 0));
    vi.stubGlobal("getComputedStyle", () => Object.assign([], { getPropertyValue: () => "" }));
    mocks.readPluginUiEntry.mockResolvedValue({ dataBase64: btoa("<!doctype html><html><body></body></html>"), contentType: "text/html" });
    mocks.subscribePluginEvents.mockResolvedValue(vi.fn());
    mocks.repushPluginConnection.mockReset().mockResolvedValue(undefined);
    mocks.reopenPluginConnection.mockResolvedValue(undefined);
    root = document.createElement("div");
    document.body.appendChild(root);
  });

  afterEach(() => {
    app?.unmount();
    root.remove();
    vi.unstubAllGlobals();
  });

  const frameRevealed = () => root.querySelector(".pointer-events-none.opacity-0") !== null;

  function frameNonce(frame: HTMLIFrameElement): string {
    const nonce = /const channelNonce = "([0-9a-f]+)";/.exec(frame.getAttribute("srcdoc") ?? "")?.[1];
    expect(nonce).toBeTruthy();
    return nonce!;
  }

  async function currentFrame(previous?: HTMLIFrameElement): Promise<HTMLIFrameElement> {
    await vi.waitFor(() => {
      const frame = root.querySelector("iframe");
      expect(frame).toBeInstanceOf(HTMLIFrameElement);
      expect(frame).not.toBe(previous);
    });
    return root.querySelector("iframe")!;
  }

  /** happy-dom fires the srcdoc load on its own; synthesize it only when it did not. */
  async function ensureFirstLoad(frame: HTMLIFrameElement) {
    await new Promise((resolve) => setTimeout(resolve, 20));
    if (!frameRevealed()) frame.dispatchEvent(new Event("load"));
    await vi.waitFor(() => expect(frameRevealed()).toBe(true));
  }

  async function mountHost() {
    app = createApp(PluginWorkbenchHost, { plugin, contribution, context: { connectionId: "connection" } });
    app.mount(root);
    await flushWorkbenchLoad();
    const frame = await currentFrame();
    await ensureFirstLoad(frame);
    const target = frame.contentWindow!;
    const nonce = frameNonce(frame);
    const postMessage = vi.spyOn(target, "postMessage").mockImplementation(() => {});
    const ready = () => window.dispatchEvent(new MessageEvent("message", { source: target, data: { source: "dbx-plugin", version: 1, type: "ready", nonce } }));
    ready();
    await vi.waitFor(() => expect(mocks.repushPluginConnection).toHaveBeenCalled());
    await new Promise((resolve) => setTimeout(resolve, 0));
    mocks.repushPluginConnection.mockClear();
    postMessage.mockClear();
    return { frame, postMessage, ready, target, nonce };
  }

  it("ignores plugin messages without the per-document channel nonce", async () => {
    const { target, postMessage } = await mountHost();

    window.dispatchEvent(new MessageEvent("message", { source: target, data: { source: "dbx-plugin", version: 1, type: "request", id: "1", method: "host.getContext", params: {} } }));
    window.dispatchEvent(new MessageEvent("message", { source: target, data: { source: "dbx-plugin", version: 1, type: "request", id: "2", method: "host.getContext", params: {}, nonce: "forged" } }));
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(postMessage).not.toHaveBeenCalled();
  });

  it("never re-initializes a frame that loads a second time and rebuilds a fresh document instead", async () => {
    const { frame, postMessage, target, nonce } = await mountHost();

    // A second load on the same element: the frame navigated away from its srcdoc.
    frame.dispatchEvent(new Event("load"));
    window.dispatchEvent(new MessageEvent("message", { source: target, data: { source: "dbx-plugin", version: 1, type: "ready", nonce } }));
    await flushWorkbenchLoad();

    // Nothing (init included) reaches whatever the old frame navigated to.
    expect(postMessage).not.toHaveBeenCalled();
    const next = await currentFrame(frame);
    expect(frameNonce(next)).not.toBe(nonce);
  });

  it("ends in an explicit reload state when the frame keeps navigating", async () => {
    let { frame } = await mountHost();
    for (let recovery = 0; recovery < 2; recovery += 1) {
      frame.dispatchEvent(new Event("load"));
      frame = await currentFrame(frame);
      await ensureFirstLoad(frame);
    }

    frame.dispatchEvent(new Event("load"));
    await flushWorkbenchLoad();

    expect(root.querySelector("iframe")).toBeNull();
    expect(root.querySelector("[data-plugin-navigation-blocked]")?.textContent).toContain("pluginPlatform.workbenchNavigationBlocked");

    (root.querySelector("[data-plugin-reload]") as HTMLElement).click();
    const reloaded = await currentFrame();
    expect(reloaded).not.toBe(frame);
  });

  it("claims OS drops over its iframe and forwards opened handles to the plugin", async () => {
    const { frame, postMessage } = await mountHost();
    const elementFromPoint = vi.spyOn(document, "elementFromPoint").mockReturnValue(frame);
    // The Rust registry hands out uuid strings; the `t` prefix stays opaque.
    mocks.openPluginLocalFile.mockResolvedValue({ handleId: "0d9f6d26-9e0e-4b1f-8f9a-2b6d3c5a7e81", name: "a.txt", size: 3, contentType: "text/plain", write: false });

    const claimed = !document.dispatchEvent(
      new CustomEvent("dbx:tauri-file-drop", {
        detail: { type: "drop", paths: ["/tmp/a.txt"], position: { x: 200, y: 200 } },
        cancelable: true,
      }),
    );

    expect(claimed).toBe(true);
    await vi.waitFor(() => {
      const posted = postMessage.mock.calls.map(([message]) => message as Record<string, unknown>);
      expect(posted.some((message) => message.type === "filedrop" && (message.files as Array<Record<string, unknown>>)?.some((file) => file.handleId === "t0d9f6d26-9e0e-4b1f-8f9a-2b6d3c5a7e81" && file.name === "a.txt"))).toBe(true);
    });
    expect(mocks.openPluginLocalFile).toHaveBeenCalledWith("sample", "/tmp/a.txt", false);
    elementFromPoint.mockRestore();
  });

  it("leaves drops outside the iframe to the host fallback", async () => {
    const { postMessage } = await mountHost();
    const elementFromPoint = vi.spyOn(document, "elementFromPoint").mockReturnValue(document.body);

    const claimed = !document.dispatchEvent(
      new CustomEvent("dbx:tauri-file-drop", {
        detail: { type: "drop", paths: ["/tmp/a.txt"], position: { x: 200, y: 200 } },
        cancelable: true,
      }),
    );

    expect(claimed).toBe(false);
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(postMessage).not.toHaveBeenCalled();
    expect(mocks.openPluginLocalFile).not.toHaveBeenCalled();
    elementFromPoint.mockRestore();
  });

  it("reports drag enter/leave state while the pointer is over the iframe", async () => {
    const { frame, postMessage } = await mountHost();
    const elementFromPoint = vi.spyOn(document, "elementFromPoint").mockReturnValue(frame);
    const payload = (type: "enter" | "leave") => new CustomEvent("dbx:tauri-file-drop", { detail: { type, position: { x: 10, y: 10 } }, cancelable: true });

    document.dispatchEvent(payload("enter"));
    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({ type: "dragstate", active: true }), "*");

    document.dispatchEvent(payload("leave"));
    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({ type: "dragstate", active: false }), "*");
    elementFromPoint.mockRestore();
  });
});
