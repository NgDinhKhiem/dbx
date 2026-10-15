<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { ArrowDown, ArrowUp, ArrowUpDown, ChevronDown, ChevronRight, LoaderCircle, X } from "@lucide/vue";
import { Button } from "@/components/ui/button";
import DiscoverDocDetail from "./DiscoverDocDetail.vue";
import DiscoverHighlightText from "./DiscoverHighlightText.vue";
import { fieldSegments, getHitFieldValue, sourceSummary, type SourceSummaryEntry } from "@/lib/elasticsearch/discover/documents";
import { isSortableField } from "@/lib/elasticsearch/discover/mapping";
import { formatDateTime } from "@/lib/elasticsearch/discover/timeRange";
import { clampColumnWidth, DISCOVER_DEFAULT_TIME_COLUMN_WIDTH, DISCOVER_TIME_COLUMN_KEY, discoverTableMinWidth } from "@/lib/elasticsearch/discover/columnWidths";
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
  /** Resized widths in px by column key (field name, `_source`, or the time column key). */
  columnWidths?: Readonly<Record<string, number>>;
}>();

const emit = defineEmits<{
  (e: "sort", field: string): void;
  (e: "toggle-column", field: string): void;
  (e: "add-filter", payload: { field: string; value: unknown; negate: boolean }): void;
  (e: "load-more"): void;
  /** `null` resets the column to its automatic width. */
  (e: "resize-column", key: string, width: number | null): void;
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

// ---- column resizing ---------------------------------------------------------
const EXPAND_COLUMN_WIDTH = 24;
const KEYBOARD_STEP = 16;
/** Width of the column being dragged, shown live before it is committed. */
const liveResize = ref<{ key: string; width: number } | null>(null);
let dragCleanup: (() => void) | null = null;

function columnWidth(key: string): number | undefined {
  if (liveResize.value?.key === key) return liveResize.value.width;
  const saved = props.columnWidths?.[key];
  if (saved !== undefined) return saved;
  return key === DISCOVER_TIME_COLUMN_KEY ? DISCOVER_DEFAULT_TIME_COLUMN_WIDTH : undefined;
}

function colStyle(key: string) {
  const width = columnWidth(key);
  return width === undefined ? undefined : { width: `${width}px` };
}

const tableStyle = computed(() => {
  const keys = [...(props.timeField ? [DISCOVER_TIME_COLUMN_KEY] : []), ...dataColumns.value];
  return { minWidth: `${discoverTableMinWidth(EXPAND_COLUMN_WIDTH, keys.map(columnWidth))}px` };
});

function headerWidth(handle: EventTarget | null): number {
  const header = handle instanceof HTMLElement ? handle.closest("th") : null;
  return header?.getBoundingClientRect().width ?? 0;
}

function startResize(event: MouseEvent, key: string) {
  if (event.button !== 0) return;
  event.preventDefault();
  event.stopPropagation();
  dragCleanup?.();
  const startX = event.clientX;
  const startWidth = columnWidth(key) ?? headerWidth(event.currentTarget);
  liveResize.value = { key, width: clampColumnWidth(startWidth) };
  const previousCursor = document.body.style.cursor;
  const previousSelect = document.body.style.userSelect;
  document.body.style.cursor = "col-resize";
  document.body.style.userSelect = "none";
  const onMove = (move: MouseEvent) => {
    liveResize.value = { key, width: clampColumnWidth(startWidth + move.clientX - startX) };
  };
  const finish = (commit: boolean) => {
    document.removeEventListener("mousemove", onMove);
    document.removeEventListener("mouseup", onUp);
    document.body.style.cursor = previousCursor;
    document.body.style.userSelect = previousSelect;
    const width = liveResize.value?.key === key ? liveResize.value.width : null;
    liveResize.value = null;
    dragCleanup = null;
    if (commit && width !== null) emit("resize-column", key, width);
  };
  const onUp = () => finish(true);
  document.addEventListener("mousemove", onMove);
  document.addEventListener("mouseup", onUp);
  dragCleanup = () => finish(false);
}

function onResizeKeydown(event: KeyboardEvent, key: string) {
  if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
  event.preventDefault();
  const step = (event.shiftKey ? 4 : 1) * KEYBOARD_STEP * (event.key === "ArrowLeft" ? -1 : 1);
  const current = columnWidth(key) ?? headerWidth(event.currentTarget);
  emit("resize-column", key, clampColumnWidth(current + step));
}

onBeforeUnmount(() => dragCleanup?.());
</script>

<template>
  <div class="h-full min-h-0 overflow-auto" data-testid="discover-doc-table" @scroll.passive="onScroll">
    <table class="w-full table-fixed border-separate border-spacing-0 text-xs" :style="tableStyle" data-testid="discover-table">
      <colgroup>
        <col :style="{ width: `${EXPAND_COLUMN_WIDTH}px` }" />
        <col v-if="timeField" :style="colStyle(DISCOVER_TIME_COLUMN_KEY)" data-testid="discover-col-time" />
        <col v-for="column in dataColumns" :key="column" :style="colStyle(column)" :data-testid="`discover-col-${column}`" />
      </colgroup>
      <thead class="sticky top-0 z-10 bg-background">
        <tr>
          <th class="border-b border-border" />
          <th v-if="timeField" class="group relative border-b border-border px-2 py-1.5 text-left font-medium whitespace-nowrap">
            <button type="button" class="inline-flex max-w-full items-center gap-1 overflow-hidden hover:text-primary" :data-testid="`discover-sort-${timeField}`" @click="emit('sort', timeField)">
              {{ t("esDiscover.time") }}
              <component :is="sortIcon(timeField)" class="size-3 shrink-0 text-muted-foreground" />
            </button>
            <span
              role="separator"
              aria-orientation="vertical"
              tabindex="0"
              :aria-label="t('esDiscover.resizeColumn', { column: t('esDiscover.time') })"
              :title="t('esDiscover.resizeColumnHint')"
              class="absolute inset-y-0 right-0 z-10 w-1.5 cursor-col-resize select-none hover:bg-primary/40 focus-visible:bg-primary/60 focus-visible:outline-none"
              :class="liveResize?.key === DISCOVER_TIME_COLUMN_KEY ? 'bg-primary/60' : ''"
              data-testid="discover-resize-time"
              @mousedown="startResize($event, DISCOVER_TIME_COLUMN_KEY)"
              @dblclick.stop="emit('resize-column', DISCOVER_TIME_COLUMN_KEY, null)"
              @keydown="onResizeKeydown($event, DISCOVER_TIME_COLUMN_KEY)"
              @click.stop
            />
          </th>
          <th v-for="column in dataColumns" :key="column" class="group relative border-b border-border px-2 py-1.5 text-left font-medium">
            <span
              role="separator"
              aria-orientation="vertical"
              tabindex="0"
              :aria-label="t('esDiscover.resizeColumn', { column: column === '_source' ? t('esDiscover.document') : column })"
              :title="t('esDiscover.resizeColumnHint')"
              class="absolute inset-y-0 right-0 z-10 w-1.5 cursor-col-resize select-none hover:bg-primary/40 focus-visible:bg-primary/60 focus-visible:outline-none"
              :class="liveResize?.key === column ? 'bg-primary/60' : ''"
              :data-testid="`discover-resize-${column}`"
              @mousedown="startResize($event, column)"
              @dblclick.stop="emit('resize-column', column, null)"
              @keydown="onResizeKeydown($event, column)"
              @click.stop
            />
            <div class="flex min-w-0 items-center gap-1 overflow-hidden">
              <button v-if="column !== '_source' && sortable(column)" type="button" class="inline-flex min-w-0 items-center gap-1 truncate font-mono hover:text-primary" :title="column" :data-testid="`discover-sort-${column}`" @click="emit('sort', column)">
                {{ column }}
                <component :is="sortIcon(column)" class="size-3 text-muted-foreground" />
              </button>
              <span v-else class="truncate font-mono" :title="column === '_source' ? undefined : column">{{ column === "_source" ? t("esDiscover.document") : column }}</span>
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
            <td v-if="timeField" class="truncate border-b border-border/60 px-2 py-1.5 font-mono whitespace-nowrap">{{ formatTime(hit) }}</td>
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
