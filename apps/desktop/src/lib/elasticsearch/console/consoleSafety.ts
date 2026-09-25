import { classifyElasticsearchRequestRisk, type ElasticsearchRequestRisk } from "@/lib/elasticsearch/elasticsearchRequestRisk";
import { productionContextForDatabase } from "@/lib/database/productionSafety";
import { consolePathSegments, type ConsoleRequest } from "@/lib/elasticsearch/console/consoleRequests";
import type { ConnectionConfig } from "@/types/database";

export interface ConsoleSafetyAssessment {
  /** Requests that write or are destructive (anything not classified as a read). */
  risky: ConsoleRequest[];
  /** Production scope of the risky requests: the whole connection, or marked indices they target. */
  production: { active: boolean; databases: string[] };
}

export function consoleRequestRisk(request: Pick<ConsoleRequest, "text">): ElasticsearchRequestRisk {
  // An unclassifiable request is treated as dangerous, never silently as a read.
  return classifyElasticsearchRequestRisk(request.text) ?? "dangerous";
}

/** Index names targeted by a request path, e.g. `/logs-a,logs-b/_delete_by_query` → ["logs-a", "logs-b"]. */
export function consoleRequestIndices(path: string): string[] {
  const first = consolePathSegments(path)[0];
  if (!first || first.startsWith("_")) return [];
  return first
    .split(",")
    .map((name) => {
      try {
        return decodeURIComponent(name).trim();
      } catch {
        return name.trim();
      }
    })
    .filter(Boolean);
}

export function assessConsoleRequests(requests: readonly ConsoleRequest[], connection: Pick<ConnectionConfig, "is_production" | "production_databases" | "db_type"> | undefined): ConsoleSafetyAssessment {
  const risky = requests.filter((request) => consoleRequestRisk(request) !== "read");
  if (!risky.length || !connection) return { risky, production: { active: false, databases: [] } };
  if (connection.is_production) return { risky, production: { active: true, databases: [] } };

  const databases = new Set<string>();
  for (const request of risky) {
    for (const index of consoleRequestIndices(request.path)) {
      const context = productionContextForDatabase(connection as ConnectionConfig, index);
      if (context.active) context.databases.forEach((database) => databases.add(database));
    }
  }
  return { risky, production: { active: databases.size > 0, databases: [...databases] } };
}
