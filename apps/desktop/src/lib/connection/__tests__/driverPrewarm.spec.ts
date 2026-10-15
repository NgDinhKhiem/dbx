import { describe, expect, it, vi } from "vitest";
import { createDriverPrewarmer, HOVER_PREWARM_COOLDOWN_MS, mqAgentPrewarmKey, selectStartupPrewarmTargets } from "@/lib/connection/driverPrewarm";
import type { ConnectionConfig } from "@/types/database";

type Conn = Pick<ConnectionConfig, "id" | "db_type" | "driver_profile" | "external_config">;

const kafka = (id: string): Conn => ({ id, db_type: "mq", driver_profile: "kafka", external_config: { systemKind: "kafka" } }) as Conn;
const rocketmq = (id: string): Conn => ({ id, db_type: "mq", driver_profile: undefined, external_config: { systemKind: "rocketmq" } }) as Conn;
const pulsar = (id: string): Conn => ({ id, db_type: "mq", driver_profile: "pulsar", external_config: { systemKind: "pulsar" } }) as Conn;
const mysql = (id: string): Conn => ({ id, db_type: "mysql" }) as Conn;

describe("mqAgentPrewarmKey", () => {
  it("returns the agent for agent-backed message queues only", () => {
    expect(mqAgentPrewarmKey(kafka("k"))).toBe("kafka");
    expect(mqAgentPrewarmKey(rocketmq("r"))).toBe("rocketmq");
    expect(mqAgentPrewarmKey(pulsar("p"))).toBeNull();
    expect(mqAgentPrewarmKey(mysql("m"))).toBeNull();
    expect(mqAgentPrewarmKey(undefined)).toBeNull();
  });
});

describe("selectStartupPrewarmTargets", () => {
  it("picks one connection per agent kind and skips kinds that are already live", () => {
    const connections = [mysql("m"), kafka("k1"), kafka("k2"), rocketmq("r1"), pulsar("p")];
    expect(selectStartupPrewarmTargets(connections, () => false)).toEqual(["k1", "r1"]);
    // A connected Kafka connection already owns a warm agent.
    expect(selectStartupPrewarmTargets(connections, (id) => id === "k2")).toEqual(["r1"]);
  });

  it("honours the spare limit", () => {
    const connections = [kafka("k"), rocketmq("r"), { ...kafka("x"), driver_profile: "rabbitmq", external_config: { systemKind: "rabbitmq" } } as Conn];
    expect(selectStartupPrewarmTargets(connections, () => false, 1)).toEqual(["k"]);
    expect(selectStartupPrewarmTargets(connections, () => false)).toHaveLength(2);
  });
});

describe("createDriverPrewarmer", () => {
  function setup(overrides: { enabled?: boolean; connected?: Set<string> } = {}) {
    let clock = 1_000;
    const prewarm = vi.fn().mockResolvedValue(true);
    const configs = new Map<string, Conn>([
      ["k", kafka("k")],
      ["m", mysql("m")],
    ]);
    const prewarmer = createDriverPrewarmer({
      prewarm,
      isEnabled: () => overrides.enabled ?? true,
      getConfig: (id) => configs.get(id),
      isConnected: (id) => overrides.connected?.has(id) ?? false,
      now: () => clock,
    });
    return { prewarm, prewarmer, advance: (ms: number) => (clock += ms) };
  }

  it("throttles hover requests per connection", () => {
    const { prewarm, prewarmer, advance } = setup();
    expect(prewarmer.request("k")).toBe(true);
    expect(prewarmer.request("k")).toBe(false);
    advance(HOVER_PREWARM_COOLDOWN_MS - 1);
    expect(prewarmer.request("k")).toBe(false);
    advance(1);
    expect(prewarmer.request("k")).toBe(true);
    expect(prewarm).toHaveBeenCalledTimes(2);
  });

  it("never warms when disabled, already connected, or for drivers without an agent", () => {
    expect(setup({ enabled: false }).prewarmer.request("k")).toBe(false);
    expect(setup({ connected: new Set(["k"]) }).prewarmer.request("k")).toBe(false);
    const { prewarm, prewarmer } = setup();
    expect(prewarmer.request("m")).toBe(false);
    expect(prewarmer.request("unknown")).toBe(false);
    expect(prewarm).not.toHaveBeenCalled();
  });

  it("warms startup targets only when enabled and swallows backend failures", async () => {
    const disabled = setup({ enabled: false });
    expect(disabled.prewarmer.requestStartup([kafka("k")])).toEqual([]);
    expect(disabled.prewarm).not.toHaveBeenCalled();

    const { prewarm, prewarmer } = setup();
    prewarm.mockRejectedValueOnce(new Error("driver not installed"));
    expect(prewarmer.requestStartup([mysql("m"), kafka("k")])).toEqual(["k"]);
    await Promise.resolve();
    expect(prewarm).toHaveBeenCalledWith("k");
    // The startup request also starts the hover cooldown.
    expect(prewarmer.request("k")).toBe(false);
  });
});
