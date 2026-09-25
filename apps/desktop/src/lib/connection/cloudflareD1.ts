import type { ConnectionConfig } from "@/types/database";
import { hasSavedSecret } from "@/lib/connection/savedSecrets";

type CloudflareD1Config = Pick<ConnectionConfig, "db_type" | "host" | "database" | "password"> & Partial<Pick<ConnectionConfig, "saved_secrets" | "cleared_secrets">>;
type MutableCloudflareD1Config = CloudflareD1Config & Pick<ConnectionConfig, "port" | "username" | "ssl" | "url_params" | "transport_layers">;

export function isCloudflareD1Connection(config: Pick<ConnectionConfig, "db_type">): boolean {
  return config.db_type === "cloudflare-d1";
}

export function hasCloudflareD1Credentials(config: CloudflareD1Config): boolean {
  // A saved (hidden) API token counts: the backend merges it back by connection id.
  return !!config.host.trim() && !!config.database?.trim() && (!!config.password.trim() || hasSavedSecret(config, "password"));
}

export function normalizeCloudflareD1Connection(config: MutableCloudflareD1Config): void {
  config.host = config.host.trim();
  config.database = config.database?.trim() || undefined;
  config.password = config.password.trim();
  config.port = 443;
  config.username = "";
  config.ssl = true;
  config.url_params = "";
  config.transport_layers = [];
}
