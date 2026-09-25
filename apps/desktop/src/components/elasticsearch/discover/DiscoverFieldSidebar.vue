<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ChevronDown, ChevronRight, CircleMinus, CirclePlus, Search } from "@lucide/vue";
import { Button } from "@/components/ui/button";
import DiscoverFieldTypeIcon from "./DiscoverFieldTypeIcon.vue";
import { computeTopValues } from "@/lib/elasticsearch/discover/documents";
import type { DiscoverField, DiscoverHit } from "@/lib/elasticsearch/discover/types";

const props = defineProps<{
  fields: readonly DiscoverField[];
  columns: readonly string[];
  hits: readonly DiscoverHit[];
  canFilter: boolean;
}>();

const emit = defineEmits<{
  (e: "toggle-column", field: string): void;
  (e: "add-filter", payload: { field: string; value: unknown; negate: boolean }): void;
}>();

const { t } = useI18n();
const search = ref("");
const expanded = ref<string | null>(null);

const matches = (field: DiscoverField) => !search.value.trim() || field.name.toLowerCase().includes(search.value.trim().toLowerCase());

const selectedFields = computed(() => props.columns.map((name) => props.fields.find((field) => field.name === name) ?? ({ name, type: "unknown", esTypes: [], searchable: false, aggregatable: false, subField: false } as DiscoverField)).filter(matches));
const availableFields = computed(() => props.fields.filter((field) => !props.columns.includes(field.name) && matches(field)));

const details = computed(() => {
  const name = expanded.value;
  if (!name) return null;
  const field = props.fields.find((candidate) => candidate.name === name);
  return computeTopValues(props.hits, name, 5, field);
});

function toggleDetails(name: string) {
  expanded.value = expanded.value === name ? null : name;
}

function canFilterField(field: DiscoverField): boolean {
  return props.canFilter && (field.searchable || field.aggregatable) && !field.unmapped;
}
</script>

<template>
  <div class="flex h-full min-h-0 flex-col" data-testid="discover-field-sidebar">
    <div class="shrink-0 border-b border-border p-2">
      <div class="flex h-7 items-center gap-1.5 rounded-md border border-input px-2 dark:bg-input/30">
        <Search class="size-3.5 text-muted-foreground" />
        <input v-model="search" class="h-full min-w-0 flex-1 bg-transparent text-xs outline-none placeholder:text-muted-foreground" :placeholder="t('esDiscover.searchFieldNames')" data-testid="discover-field-search" />
      </div>
    </div>
    <div class="min-h-0 flex-1 overflow-y-auto py-1 text-xs">
      <template
        v-for="section in [
          { key: 'selected', items: selectedFields },
          { key: 'available', items: availableFields },
        ]"
        :key="section.key"
      >
        <div v-if="section.key === 'available' || section.items.length > 0" class="flex items-center justify-between px-3 pt-2 pb-1 text-[11px] font-semibold tracking-wide text-muted-foreground uppercase">
          <span>{{ section.key === "selected" ? t("esDiscover.selectedFields") : t("esDiscover.availableFields") }}</span>
          <span class="font-normal">{{ section.items.length }}</span>
        </div>
        <div v-for="field in section.items" :key="`${section.key}:${field.name}`">
          <div
            class="group flex h-7 cursor-pointer items-center gap-1.5 px-2 hover:bg-muted"
            :class="expanded === field.name ? 'bg-muted' : ''"
            :data-testid="`discover-field-${field.name}`"
            :title="`${field.name} (${field.esTypes.join(', ') || t('esDiscover.unmapped')})`"
            @click="toggleDetails(field.name)"
          >
            <component :is="expanded === field.name ? ChevronDown : ChevronRight" class="size-3 shrink-0 text-muted-foreground" />
            <DiscoverFieldTypeIcon :type="field.type" />
            <span class="min-w-0 flex-1 truncate font-mono" :class="field.unmapped ? 'text-muted-foreground italic' : ''">{{ field.name }}</span>
            <Button variant="ghost" size="xs" class="h-5 px-1.5 text-[10px] opacity-0 group-hover:opacity-100 focus-visible:opacity-100" :class="columns.includes(field.name) ? 'opacity-100' : ''" :data-testid="`discover-toggle-column-${field.name}`" @click.stop="emit('toggle-column', field.name)">
              {{ columns.includes(field.name) ? t("esDiscover.removeColumn") : t("esDiscover.addColumn") }}
            </Button>
          </div>
          <div v-if="expanded === field.name && details" class="mx-2 mb-2 rounded-md border border-border bg-background p-2" :data-testid="`discover-field-details-${field.name}`">
            <div class="mb-1.5 font-medium">{{ t("esDiscover.topValues", { count: 5 }) }}</div>
            <div v-if="details.values.length === 0" class="text-muted-foreground">{{ t("esDiscover.noValuesInSample") }}</div>
            <div v-for="entry in details.values" :key="entry.label" class="mb-1.5" data-testid="discover-top-value">
              <div class="flex items-center gap-1">
                <span class="min-w-0 flex-1 truncate font-mono" :title="entry.label">{{ entry.label === "" ? t("esDiscover.emptyString") : entry.label }}</span>
                <span class="shrink-0 tabular-nums text-muted-foreground">{{ entry.percent.toFixed(1) }}%</span>
                <template v-if="canFilterField(field)">
                  <button
                    type="button"
                    class="rounded p-0.5 text-muted-foreground hover:bg-muted hover:text-foreground"
                    :title="t('esDiscover.filterFor')"
                    :aria-label="t('esDiscover.filterFor')"
                    data-testid="discover-top-value-include"
                    @click="emit('add-filter', { field: field.name, value: entry.value, negate: false })"
                  >
                    <CirclePlus class="size-3.5" />
                  </button>
                  <button
                    type="button"
                    class="rounded p-0.5 text-muted-foreground hover:bg-muted hover:text-foreground"
                    :title="t('esDiscover.filterOut')"
                    :aria-label="t('esDiscover.filterOut')"
                    data-testid="discover-top-value-exclude"
                    @click="emit('add-filter', { field: field.name, value: entry.value, negate: true })"
                  >
                    <CircleMinus class="size-3.5" />
                  </button>
                </template>
              </div>
              <div class="mt-0.5 h-1 overflow-hidden rounded-full bg-muted">
                <div class="h-full rounded-full bg-primary/70" :style="{ width: `${Math.min(100, entry.percent)}%` }" />
              </div>
            </div>
            <div class="mt-1 text-[11px] text-muted-foreground">{{ t("esDiscover.existsIn", { exists: details.exists, total: details.total }) }}</div>
            <Button variant="outline" size="xs" class="mt-2 w-full text-[11px]" @click="emit('toggle-column', field.name)">
              {{ columns.includes(field.name) ? t("esDiscover.removeColumn") : t("esDiscover.addColumn") }}
            </Button>
          </div>
        </div>
      </template>
      <div v-if="fields.length === 0" class="px-3 py-4 text-center text-muted-foreground">{{ t("esDiscover.noFields") }}</div>
    </div>
  </div>
</template>
