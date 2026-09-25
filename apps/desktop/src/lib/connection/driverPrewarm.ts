import type { ConnectionConfig } from "@/types/database";

/**
 * Driver runtime prewarm policy.
 *
 * Agent-backed message queues (Kafka, RocketMQ, RabbitMQ) run a JVM agent whose
 * start-up is paid on the first connect. Prewarm only asks the backend to start
 * that process in the background; it never connects to a broker or database.
 */

/** Delay after startup before warming, so it does not compete with app init. */
export const STARTUP_PREWARM_DELAY_MS = 4_000;
/** Minimum time between two hover prewarm requests for the same connection. */
export const HOVER_PREWARM_COOLDOWN_MS = 60_000;
/** The backend keeps at most this many spare agent processes. */
export const MAX_STARTUP_PREWARM_TARGETS = 2;

type PrewarmConnection = Pick<ConnectionConfig, "id" | "db_type" | "driver_profile" | "external_config">;

const AGENT_MQ_SYSTEMS = new Set(["kafka", "rocketmq", "rabbitmq"]);

/** Agent that prewarming would start for this connection, or `null` when it has none. */
export function mqAgentPrewarmKey(config: PrewarmConnection | undefined): string | null {
  if (!config || config.db_type !== "mq") return null;
  const profile = config.driver_profile?.trim().toLowerCase();
  if (profile && AGENT_MQ_SYSTEMS.has(profile)) return profile;
  const external = config.external_config as { systemKind?: unknown } | null | undefined;
  const kind = typeof external?.systemKind === "string" ? external.systemKind.toLowerCase() : "";
  return AGENT_MQ_SYSTEMS.has(kind) ? kind : null;
}

/**
 * Connections to warm at startup: one per agent kind (a spare is per agent, not
 * per connection), skipping kinds that already have a live connection.
 */
export function selectStartupPrewarmTargets(connections: readonly PrewarmConnection[], isConnected: (connectionId: string) => boolean, limit = MAX_STARTUP_PREWARM_TARGETS): string[] {
  const liveKinds = new Set<string>();
  for (const connection of connections) {
    const key = mqAgentPrewarmKey(connection);
    if (key && isConnected(connection.id)) liveKinds.add(key);
  }
  const targets: string[] = [];
  const chosenKinds = new Set<string>();
  for (const connection of connections) {
    if (targets.length >= limit) break;
    const key = mqAgentPrewarmKey(connection);
    if (!key || liveKinds.has(key) || chosenKinds.has(key)) continue;
    chosenKinds.add(key);
    targets.push(connection.id);
  }
  return targets;
}

export interface DriverPrewarmerOptions {
  prewarm: (connectionId: string) => Promise<unknown>;
  isEnabled: () => boolean;
  getConfig: (connectionId: string) => PrewarmConnection | undefined;
  isConnected: (connectionId: string) => boolean;
  now?: () => number;
  cooldownMs?: number;
}

export interface DriverPrewarmer {
  /** Warm the driver for one connection (hover / expand intent). Returns whether a request was sent. */
  request: (connectionId: string) => boolean;
  /** Warm drivers for saved connections after startup. Returns the connection ids requested. */
  requestStartup: (connections: readonly PrewarmConnection[]) => string[];
}

export function createDriverPrewarmer(options: DriverPrewarmerOptions): DriverPrewarmer {
  const now = options.now ?? Date.now;
  const cooldownMs = options.cooldownMs ?? HOVER_PREWARM_COOLDOWN_MS;
  const lastRequestedAt = new Map<string, number>();

  const send = (connectionId: string) => {
    lastRequestedAt.set(connectionId, now());
    // Best effort: a missing driver or an old backend simply keeps the cold path.
    void options.prewarm(connectionId).catch(() => {});
  };

  return {
    request(connectionId) {
      if (!options.isEnabled() || options.isConnected(connectionId)) return false;
      if (!mqAgentPrewarmKey(options.getConfig(connectionId))) return false;
      const last = lastRequestedAt.get(connectionId);
      if (last !== undefined && now() - last < cooldownMs) return false;
      send(connectionId);
      return true;
    },
    requestStartup(connections) {
      if (!options.isEnabled()) return [];
      const targets = selectStartupPrewarmTargets(connections, options.isConnected);
      for (const connectionId of targets) send(connectionId);
      return targets;
    },
  };
}
