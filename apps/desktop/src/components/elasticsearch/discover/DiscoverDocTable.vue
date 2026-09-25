<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { ArrowDown, ArrowUp, ArrowUpDown, ChevronDown, ChevronRight, LoaderCircle, X } from "@lucide/vue";
import { Button } from "@/components/ui/button";
import DiscoverDocDetail from "./DiscoverDocDetail.vue";
import DiscoverHighlightText from "./DiscoverHighlightText.vue";
import { fieldSegments, getHitFieldValue, sourceSummary, type SourceSummaryEntry } from "@/lib/elasticsearch/discover/documents";
import { isSortableField } from "@/lib/elasticsearch/discover/mapping";
import { formatDateTime } from "@/lib/elasticsearch/discover/timeRange";
import type { DiscoverField, DiscoverHit, DiscoverSort } from "@/lib/elasticsearch/discover/types";

const RENDER_STEP = 50;

const props = defineProps<{
  hits: readonly DiscoverHit[];
  columns: readonly string[];
  timeField: string;
  fields: readonly DiscoverField[];
  sort: readonly DiscoverSort[];
  canFilter: boolean;
  canLoadMore: boolean;
  loadingMore: boolean;
  sampleLimitReached: boolean;
}>();

const emit = defineEmits<{
  (e: "sort", field: string): void;
  (e: "toggle-column", field: string): void;
  (e: "add-filter", payload: { field: string; value: unknown; negate: boolean }): void;
  (e: "load-more"): void;
}>();

const { t } = useI18n();
const expanded = ref(new Set<string>());
/** Progressive rendering: only the first `renderLimit` rows are in the DOM. */
const renderLimit = ref(RENDER_STEP);

watch(
  () => props.hits,
  (hits, previous) => {
    // A new search (not a "load more" append) resets expansion and the render window.
    if (!previous || hits.length < previous.length || hits[0] !== previous[0]) {
      expanded.value = new Set();
      renderLimit.value = RENDER_STEP;
    }
  },
);

const fieldByName = computed(() => new Map(props.fields.map((field) => [field.name, field])));
const dataColumns = computed(() => (props.columns.length > 0 ? [...props.columns] : ["_source"]));
const visibleHits = computed(() => props.hits.slice(0, renderLimit.value));
const primarySort = computed(() => props.sort[0]);

// Hits are immutable once loaded, so their summaries can be cached per object.
const summaryCache = new WeakMap<DiscoverHit, SourceSummaryEntry[]>();
function summaryFor(hit: DiscoverHit): SourceSummaryEntry[] {
  let summary = summaryCache.get(hit);
  if (!summary) {
    summary = sourceSummary(hit);
    summaryCache.set(hit, summary);
  }
  return summary;
}

function rowKey(hit: DiscoverHit): string {
  return `${hit._index}\u0000${hit._id}`;
}

function toggleRow(hit: DiscoverHit) {
  const key = rowKey(hit);
  const next = new Set(expanded.value);
  if (next.has(key)) next.delete(key);
  else next.add(key);
  expanded.value = next;
}

function formatTime(hit: DiscoverHit): string {
  const value = getHitFieldValue(hit, props.timeField);
  const first = Array.isArray(value) ? value[0] : value;
  if (typeof first === "string" || typeof first === "number") {
    const date = new Date(first);
    if (!Number.isNaN(date.getTime())) return formatDateTime(date);
    return String(first);
  }
  return "-";
}

function sortable(column: string): boolean {
  return column === props.timeField || isSortableField(fieldByName.value.get(column));
}

function sortIcon(column: string) {
  if (primarySort.value?.field !== column) return ArrowUpDown;
  return primarySort.value.direction === "asc" ? ArrowUp : ArrowDown;
}

function onScroll(event: Event) {
  const target = event.target as HTMLElement;
  if (renderLimit.value < props.hits.length && target.scrollTop + target.clientHeight >= target.scrollHeight - 400) {
    renderLimit.value = Math.min(props.hits.length, renderLimit.value + RENDER_STEP);
  }
}

function showAllRendered() {
  renderLimit.value = props.hits.length;
}
</script>

<template>
  <div class="h-full min-h-0 overflow-auto" data-testid="discover-doc-table" @scroll.passive="onScroll">
    <table class="w-full border-separate border-spacing-0 text-xs">
      <thead class="sticky top-0 z-10 bg-background">
        <tr>
          <th class="w-6 border-b border-border" />
          <th v-if="timeField" class="w-48 border-b border-border px-2 py-1.5 text-left font-medium whitespace-nowrap">
            <button type="button" class="inline-flex items-center gap-1 hover:text-primary" :data-testid="`discover-sort-${timeField}`" @click="emit('sort', timeField)">
              {{ t("esDiscover.time") }}
              <component :is="sortIcon(timeField)" class="size-3 text-muted-foreground" />
            </button>
          </th>
          <th v-for="column in dataColumns" :key="column" class="group border-b border-border px-2 py-1.5 text-left font-medium">
            <div class="flex items-center gap-1">
              <button v-if="column !== '_source' && sortable(column)" type="button" class="inline-flex items-center gap-1 font-mono hover:text-primary" :data-testid="`discover-sort-${column}`" @click="emit('sort', column)">
                {{ column }}
                <component :is="sortIcon(column)" class="size-3 text-muted-foreground" />
              </button>
              <span v-else class="font-mono">{{ column === "_source" ? t("esDiscover.document") : column }}</span>
              <button v-if="column !== '_source'" type="button" class="rounded p-0.5 text-muted-foreground opacity-0 group-hover:opacity-100 hover:text-foreground" :title="t('esDiscover.removeColumn')" :aria-label="t('esDiscover.removeColumn')" @click="emit('toggle-column', column)">
                <X class="size-3" />
              </button>
            </div>
          </th>
        </tr>
      </thead>
      <tbody>
        <template v-for="hit in visibleHits" :key="rowKey(hit)">
          <tr class="cursor-pointer align-top hover:bg-muted/40" :class="expanded.has(rowKey(hit)) ? 'bg-muted/30' : ''" data-testid="discover-row" @click="toggleRow(hit)">
            <td class="border-b border-border/60 py-1.5 pl-1.5 text-muted-foreground">
              <component :is="expanded.has(rowKey(hit)) ? ChevronDown : ChevronRight" class="size-3.5" />
            </td>
            <td v-if="timeField" class="border-b border-border/60 px-2 py-1.5 font-mono whitespace-nowrap">{{ formatTime(hit) }}</td>
            <td v-for="column in dataColumns" :key="column" class="border-b border-border/60 px-2 py-1.5">
              <div v-if="column === '_source'" class="line-clamp-3 font-mono leading-5 break-all" data-testid="discover-source-summary">
                <span v-for="entry in summaryFor(hit)" :key="entry.field" class="mr-2">
                  <span class="rounded-sm bg-muted px-1 font-semibold text-muted-foreground">{{ entry.field }}:</span>
                  <DiscoverHighlightText :segments="entry.segments" />
                </span>
              </div>
              <div v-else class="line-clamp-3 font-mono break-all">
                <DiscoverHighlightText :segments="fieldSegments(hit, column, 500, fieldByName.get(column))" />
              </div>
            </td>
          </tr>
          <tr v-if="expanded.has(rowKey(hit))">
            <td :colspan="dataColumns.length + (timeField ? 2 : 1)" class="border-b border-border p-0">
              <DiscoverDocDetail :hit="hit" :fields="fields" :columns="columns" :can-filter="canFilter" @toggle-column="emit('toggle-column', $event)" @add-filter="emit('add-filter', $event)" />
            </td>
          </tr>
        </template>
      </tbody>
    </table>
    <div class="flex flex-col items-center gap-2 py-3 text-xs text-muted-foreground">
      <button v-if="renderLimit < hits.length" type="button" class="hover:text-foreground" @click="showAllRendered">{{ t("esDiscover.showRenderedRows", { shown: renderLimit, total: hits.length }) }}</button>
      <template v-else>
        <Button v-if="canLoadMore" variant="outline" size="sm" :disabled="loadingMore" data-testid="discover-load-more" @click="emit('load-more')">
          <LoaderCircle v-if="loadingMore" class="size-3.5 animate-spin" />
          {{ t("esDiscover.loadMore") }}
        </Button>
        <span v-else-if="sampleLimitReached">{{ t("esDiscover.sampleLimit", { count: hits.length }) }}</span>
      </template>
    </div>
  </div>
</template>
