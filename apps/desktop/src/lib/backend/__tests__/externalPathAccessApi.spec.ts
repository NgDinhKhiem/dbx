import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: mocks.invoke,
}));

import { pickExternalDirectory, pickExternalFiles, requestExternalPathAccess } from "@/lib/backend/tauri";
import { redisPubSubWebSocketUrl } from "@/lib/backend/redisPubSubUrl";

describe("external path access API", () => {
  beforeEach(() => {
    mocks.invoke.mockReset();
  });

  it("opens the backend file picker so the selection is granted", async () => {
    mocks.invoke.mockResolvedValue(["/tmp/a.sql"]);

    await expect(pickExternalFiles({ filterName: "SQL", extensions: ["sql"] })).resolves.toEqual(["/tmp/a.sql"]);
    expect(mocks.invoke).toHaveBeenCalledWith("pick_external_files", { multiple: false, filterName: "SQL", extensions: ["sql"], title: null });
  });

  it("opens the backend folder picker", async () => {
    mocks.invoke.mockResolvedValue("/tmp/sql");

    await expect(pickExternalDirectory()).resolves.toBe("/tmp/sql");
    expect(mocks.invoke).toHaveBeenCalledWith("pick_external_directory", { title: null });
  });

  it("does not prompt for an empty access request", async () => {
    await expect(requestExternalPathAccess([], true)).resolves.toBe(true);
    expect(mocks.invoke).not.toHaveBeenCalled();

    mocks.invoke.mockResolvedValue(true);
    await expect(requestExternalPathAccess(["/tmp/sql"], true)).resolves.toBe(true);
    expect(mocks.invoke).toHaveBeenCalledWith("request_external_path_access", { paths: ["/tmp/sql"], directory: true });
  });
});

describe("Redis PubSub WebSocket URL", () => {
  it("carries the per-launch token and encodes every parameter", () => {
    const url = new URL(redisPubSubWebSocketUrl({ port: 51234, token: "abc123" }, "conn a&b", true));

    expect(url.protocol).toBe("ws:");
    expect(url.host).toBe("127.0.0.1:51234");
    expect(url.pathname).toBe("/api/redis/pubsub/ws");
    expect(url.searchParams.get("connectionId")).toBe("conn a&b");
    expect(url.searchParams.get("monitor")).toBe("true");
    expect(url.searchParams.get("token")).toBe("abc123");
  });
});
