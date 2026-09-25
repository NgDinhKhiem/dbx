import { beforeEach, describe, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  elasticsearchClusterInfo: vi.fn(),
  elasticsearchRawRequest: vi.fn(),
}));

vi.mock("@/lib/backend/api", () => backend);

import { detectDistribution } from "../discoverApi";

describe("detectDistribution", () => {
  beforeEach(() => {
    backend.elasticsearchClusterInfo.mockReset();
    backend.elasticsearchRawRequest.mockReset();
  });

  it("uses the backend's cached cluster info without a raw round trip", async () => {
    backend.elasticsearchClusterInfo.mockResolvedValue({ distribution: "opensearch", version: "1.3.19" });
    await expect(detectDistribution("es")).resolves.toBe("opensearch");
    backend.elasticsearchClusterInfo.mockResolvedValue({ distribution: "elasticsearch", version: "8.15.0" });
    await expect(detectDistribution("es")).resolves.toBe("elasticsearch");
    expect(backend.elasticsearchRawRequest).not.toHaveBeenCalled();
  });

  it("falls back to GET / when cluster info is unavailable", async () => {
    backend.elasticsearchClusterInfo.mockResolvedValueOnce({ distribution: null, version: null }).mockRejectedValueOnce(new Error("old backend"));
    backend.elasticsearchRawRequest.mockResolvedValue({ status: 200, body: JSON.stringify({ version: { distribution: "opensearch" } }), tookMs: 1 });
    await expect(detectDistribution("es")).resolves.toBe("opensearch");
    await expect(detectDistribution("es")).resolves.toBe("opensearch");
    expect(backend.elasticsearchRawRequest).toHaveBeenCalledTimes(2);
    expect(backend.elasticsearchRawRequest).toHaveBeenCalledWith("es", { method: "GET", path: "/" });
  });
});
