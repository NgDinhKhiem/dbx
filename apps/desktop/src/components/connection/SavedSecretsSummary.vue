<script setup lang="ts">
import { computed, type HTMLAttributes } from "vue";
import { useI18n } from "vue-i18n";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { transportLayerSecretSegment } from "@/lib/connection/savedSecrets";
import type { PluginFormField, TransportLayerConfig } from "@/types/database";

/**
 * Lists the secrets stored for the edited connection. Their values never reach
 * the frontend, so each one can only be kept (field left blank), replaced
 * (new value typed) or explicitly cleared here.
 */
const props = defineProps<{
  savedSecrets: readonly string[];
  clearedSecrets: readonly string[];
  transportLayers?: readonly TransportLayerConfig[];
  pluginFields?: readonly PluginFormField[];
  labelClass?: HTMLAttributes["class"];
}>();

const emit = defineEmits<{
  clear: [path: string];
  restore: [path: string];
}>();

const { t } = useI18n();

const EXTERNAL_LABEL_KEYS: Record<string, string> = {
  "auth.token": "connection.mqToken",
  "auth.password": "connection.password",
  "auth.value": "connection.mqApiKeyValue",
  "auth.clientSecret": "connection.mqOauthClientSecret",
  "tokenSigning.key": "connection.mqTokenSigningKey",
  "rnacosConsoleAuth.password": "connection.nacosConsolePassword",
  "tls.truststore_password": "connection.cassandraTruststorePassword",
  "tls.keystore_password": "connection.cassandraKeystorePassword",
};

function layerLabel(segment: string): string {
  const layers = props.transportLayers ?? [];
  const index = layers.findIndex((layer, layerIndex) => transportLayerSecretSegment(layer, layerIndex) === segment);
  const layer = layers[index];
  if (!layer) return segment;
  return layer.name?.trim() || `${layer.type === "ssh" ? "SSH" : layer.type === "proxy" ? "Proxy" : "HTTP"} #${index + 1}`;
}

function layerFieldLabel(segment: string, field: string): string {
  const layer = (props.transportLayers ?? []).find((candidate, index) => transportLayerSecretSegment(candidate, index) === segment);
  if (field === "key_passphrase") return t("connection.sshKeyPassphrase");
  if (field === "token") return t("connection.httpTunnelToken");
  return layer?.type === "proxy" ? t("connection.proxyPassword") : t("connection.sshPassword");
}

function secretLabel(path: string): string {
  switch (path) {
    case "password":
      return t("connection.password");
    case "redis_sentinel_password":
      return t("connection.redisSentinelPassword");
    case "connection_string":
      return t("connection.savedSecretConnectionString");
    case "init_script":
      return t("connection.initScript");
    case "url_params":
      return t("connection.savedSecretUrlParams");
  }
  if (path.startsWith("transport_layers.")) {
    const field = path.slice(path.lastIndexOf(".") + 1);
    const segment = path.slice("transport_layers.".length, path.lastIndexOf("."));
    return t("connection.savedSecretLayerField", { layer: layerLabel(segment), field: layerFieldLabel(segment, field) });
  }
  if (path.startsWith("external_config.")) {
    const key = EXTERNAL_LABEL_KEYS[path.slice("external_config.".length)];
    return key ? t(key) : path;
  }
  if (path.startsWith("connection_secrets.")) {
    const key = path.slice("connection_secrets.".length);
    const field = props.pluginFields?.find((candidate) => candidate.key === key);
    return field?.label || t("connection.savedSecretPluginField", { key });
  }
  return path;
}

const rows = computed(() => [...props.savedSecrets.filter((path) => !props.clearedSecrets.includes(path)).map((path) => ({ path, cleared: false })), ...props.clearedSecrets.map((path) => ({ path, cleared: true }))]);
</script>

<template>
  <div v-if="rows.length" class="grid grid-cols-4 items-start gap-4" data-testid="saved-secrets-summary">
    <Label :class="labelClass">{{ t("connection.savedSecretsTitle") }}</Label>
    <div class="col-span-3 space-y-1.5">
      <ul class="m-0 grid list-none gap-1 p-0">
        <li v-for="row in rows" :key="row.path" class="flex min-w-0 items-center justify-between gap-2 rounded-md border border-border px-2 py-1 text-xs" :data-secret-path="row.path">
          <span class="min-w-0 truncate" :class="row.cleared ? 'text-muted-foreground line-through' : ''">{{ secretLabel(row.path) }}</span>
          <span class="flex shrink-0 items-center gap-2">
            <span v-if="row.cleared" class="text-muted-foreground">{{ t("connection.savedSecretCleared") }}</span>
            <Button v-if="row.cleared" type="button" size="sm" variant="ghost" class="h-6 px-2 text-xs" @click="emit('restore', row.path)">{{ t("connection.savedSecretUndo") }}</Button>
            <Button v-else type="button" size="sm" variant="ghost" class="h-6 px-2 text-xs" @click="emit('clear', row.path)">{{ t("connection.savedSecretClear") }}</Button>
          </span>
        </li>
      </ul>
      <p class="m-0 text-xs leading-5 text-muted-foreground">{{ t("connection.savedSecretsHint") }}</p>
    </div>
  </div>
</template>
