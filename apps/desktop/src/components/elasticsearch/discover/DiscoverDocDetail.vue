<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { CircleMinus, CirclePlus, Columns3, Copy } from "@lucide/vue";
import DiscoverFieldTypeIcon from "./DiscoverFieldTypeIcon.vue";
import DiscoverHighlightText from "./DiscoverHighlightText.vue";
import { copyToClipboard } from "@/lib/common/clipboard";
import { fieldSegments, flattenHit, getHitFieldValue, hitJson } from "@/lib/elasticsearch/discover/documents";
import type { DiscoverField, DiscoverFieldType, DiscoverHit } from "@/lib/elasticsearch/discover/types";

const props = defineProps<{
  hit: DiscoverHit;
  fields: readonly DiscoverField[];
  columns: readonly string[];
  canFilter: boolean;
}>();

const emit = defineEmits<{
  (e: "toggle-column", field: string): void;
  (e: "add-filter", payload: { field: string; value: unknown; negate: boolean }): void;
}>();

const { t } = useI18n();
const view = ref<"table" | "json">("table");
const copied = ref(false);

const fieldByName = computed(() => new Map(props.fields.map((field) => [field.name, field])));

const rows = computed(() => {
  const names = [...Object.keys(flattenHit(props.hit)).sort((a, b) => a.localeCompare(b)), "_id", "_index"];
  return names.map((name) => {
    const field = fieldByName.value.get(name);
    const type: DiscoverFieldType = field?.type ?? "unknown";
    return {
      name,
      type,
      filterable: props.canFilter && Boolean(field && !field.unmapped && (field.searchable || field.aggregatable)),
      value: getHitFieldValue(props.hit, name),
      segments: fieldSegments(props.hit, name, 5000),
    };
  });
});

const json = computed(() => hitJson(props.hit));

async function copyJson() {
  try {
    await copyToClipboard(json.value);
    copied.value = true;
    window.setTimeout(() => (copied.value = false), 1200);
  } catch {
    copied.value = false;
  }
}
</script>

<template>
  <div class="border-l-2 border-primary/50 bg-muted/20 px-3 py-2" data-testid="discover-doc-detail">
    <div class="mb-2 flex items-center gap-1 text-xs">
      <button type="button" class="rounded px-2 py-0.5" :class="view === 'table' ? 'bg-background font-medium shadow-sm ring-1 ring-border' : 'text-muted-foreground hover:text-foreground'" data-testid="discover-detail-table-tab" @click="view = 'table'">
        {{ t("esDiscover.tableView") }}
      </button>
      <button type="button" class="rounded px-2 py-0.5" :class="view === 'json' ? 'bg-background font-medium shadow-sm ring-1 ring-border' : 'text-muted-foreground hover:text-foreground'" data-testid="discover-detail-json-tab" @click="view = 'json'">
        {{ t("esDiscover.jsonView") }}
      </button>
      <span class="ml-auto truncate font-mono text-[11px] text-muted-foreground">{{ hit._index }} / {{ hit._id }}</span>
    </div>
    <table v-if="view === 'table'" class="w-full table-fixed text-xs">
      <colgroup>
        <col class="w-20" />
        <col class="w-56" />
        <col />
      </colgroup>
      <tbody>
        <tr v-for="row in rows" :key="row.name" class="group border-t border-border/50 align-top hover:bg-muted/40" :data-testid="`discover-detail-row-${row.name}`">
          <td class="py-1 pr-1">
            <div class="flex items-center gap-0.5 opacity-40 group-hover:opacity-100">
              <template v-if="row.filterable && row.value !== undefined">
                <button type="button" class="rounded p-0.5 hover:bg-muted" :title="t('esDiscover.filterFor')" :aria-label="t('esDiscover.filterFor')" @click="emit('add-filter', { field: row.name, value: row.value, negate: false })">
                  <CirclePlus class="size-3.5" />
                </button>
                <button type="button" class="rounded p-0.5 hover:bg-muted" :title="t('esDiscover.filterOut')" :aria-label="t('esDiscover.filterOut')" @click="emit('add-filter', { field: row.name, value: row.value, negate: true })">
                  <CircleMinus class="size-3.5" />
                </button>
              </template>
              <button type="button" class="rounded p-0.5 hover:bg-muted" :class="columns.includes(row.name) ? 'text-primary' : ''" :title="t('esDiscover.toggleColumn')" :aria-label="t('esDiscover.toggleColumn')" @click="emit('toggle-column', row.name)">
                <Columns3 class="size-3.5" />
              </button>
            </div>
          </td>
          <td class="py-1 pr-2">
            <div class="flex min-w-0 items-center gap-1.5">
              <DiscoverFieldTypeIcon :type="row.type" />
              <span class="truncate font-mono font-medium" :title="row.name">{{ row.name }}</span>
            </div>
          </td>
          <td class="py-1 font-mono break-all whitespace-pre-wrap"><DiscoverHighlightText :segments="row.segments" /></td>
        </tr>
      </tbody>
    </table>
    <div v-else class="relative">
      <button type="button" class="absolute top-1 right-1 inline-flex items-center gap-1 rounded bg-background px-1.5 py-0.5 text-[11px] ring-1 ring-border hover:bg-muted" @click="copyJson">
        <Copy class="size-3" />
        {{ copied ? t("esDiscover.copied") : t("esDiscover.copy") }}
      </button>
      <pre class="max-h-[32rem] overflow-auto rounded-md bg-background p-2 font-mono text-[11px] leading-relaxed ring-1 ring-border" data-testid="discover-detail-json">{{ json }}</pre>
    </div>
  </div>
</template>
