<script setup lang="ts">
import { computed, markRaw, onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";
import { useI18n } from "vue-i18n";
import { LoaderCircle, RefreshCw, SearchX } from "@lucide/vue";
import { Button } from "@/components/ui/button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import ErrorBanner from "@/components/ui/ErrorBanner.vue";
import DiscoverDocTable from "./DiscoverDocTable.vue";
import DiscoverFieldSidebar from "./DiscoverFieldSidebar.vue";
import DiscoverFilterBar from "./DiscoverFilterBar.vue";
import DiscoverHistogram from "./DiscoverHistogram.vue";
import DiscoverIndexPatternInput from "./DiscoverIndexPatternInput.vue";
import DiscoverTabularResult from "./DiscoverTabularResult.vue";
import DiscoverTimeRangePicker from "./DiscoverTimeRangePicker.vue";
import { detectDistribution, discoverRequest, encodeIndexPattern, isSuccessStatus } from "@/lib/elasticsearch/discover/discoverApi";
import { DqlSyntaxError, formatDqlErrorPointer } from "@/lib/elasticsearch/discover/dql";
import { errorFromUnknown, extractErrorInfo, type DiscoverErrorInfo } from "@/lib/elasticsearch/discover/errors";
import { dateFieldNames, defaultTimeField, fieldsFromMappingResponse, withUnmappedFields } from "@/lib/elasticsearch/discover/mapping";
import { buildSearchBody, createFilterId, effectiveSort, INITIAL_PAGE_SIZE, MAX_SAMPLE_SIZE, readHistogramBuckets, readTotalHits, type HistogramBucket, type SearchBodyOptions } from "@/lib/elasticsearch/discover/requestBuilder";
import { buildPplRequest, buildSqlRequest, parseTabularResponse } from "@/lib/elasticsearch/discover/sqlPpl";
import { DEFAULT_TIME_RANGE, formatDateTime, resolveTimeRange } from "@/lib/elasticsearch/discover/timeRange";
import type { ClusterDistribution, DiscoverField, DiscoverFilter, DiscoverHit, DiscoverQueryLanguage, DiscoverSort, DiscoverState, DiscoverTimeRange, TabularResult } from "@/lib/elasticsearch/discover/types";

const props = defineProps<{
  connectionId: string;
  initialIndexPattern?: string;
  initialState?: DiscoverState;
}>();

const emit = defineEmits<{ (e: "state-change", state: DiscoverState): void }>();

const { t } = useI18n();

const NO_TIME_FIELD = "__none__";
const LANGUAGES: DiscoverQueryLanguage[] = ["dql", "lucene", "ppl", "sql"];

function cloneFilters(filters: readonly DiscoverFilter[]): DiscoverFilter[] {
  return filters.map((filter) => ({ ...filter, range: filter.range ? { ...filter.range } : undefined }));
}

// ---- persisted view state -------------------------------------------------
const initial = props.initialState;
const indexPattern = ref(initial?.indexPattern || props.initialIndexPattern || "");
/** undefined = default from mapping, "" = no time field. */
const timeFieldChoice = ref<string | undefined>(initial?.timeField);
const timeRange = ref<DiscoverTimeRange>({ ...(initial?.timeRange ?? DEFAULT_TIME_RANGE) });
const language = ref<DiscoverQueryLanguage>(initial?.language && LANGUAGES.includes(initial.language) ? initial.language : "dql");
const queryDraft = ref(initial?.query ?? "");
const filters = ref<DiscoverFilter[]>(cloneFilters(initial?.filters ?? []));
const columns = ref<string[]>([...(initial?.columns ?? [])]);
const sort = ref<DiscoverSort[]>((initial?.sort ?? []).map((entry) => ({ ...entry })));

// ---- cluster metadata -----------------------------------------------------
let distributionPromise: Promise<ClusterDistribution> | null = null;
const indices = shallowRef<string[]>([]);
const aliases = shallowRef<string[]>([]);
const indicesLoading = ref(false);
let indicesLoadedAt = 0;
const mappingFields = shallowRef<DiscoverField[]>([]);
const mappingLoaded = ref(false);
const mappingLoading = ref(false);
let mappingSeq = 0;

// ---- results ----------------------------------------------------------------
const hits = shallowRef<DiscoverHit[]>([]);
const total = ref<{ value: number; relation: "eq" | "gte" } | null>(null);
const tookMs = ref<number | null>(null);
const buckets = shallowRef<HistogramBucket[]>([]);
const histogramInterval = shallowRef<{ expression: string; ms: number } | null>(null);
const tabular = shallowRef<TabularResult | null>(null);
const loading = ref(false);
const loadingMore = ref(false);
const error = shallowRef<DiscoverErrorInfo | null>(null);
const dqlError = shallowRef<DqlSyntaxError | null>(null);
const hasSearched = ref(false);
const searchedRange = shallowRef<{ from: Date; to: Date } | null>(null);
let searchSeq = 0;
let lastSearchOptions: SearchBodyOptions | null = null;
let disposed = false;

const docMode = computed(() => language.value === "dql" || language.value === "lucene");
const dateFields = computed(() => dateFieldNames(mappingFields.value));
const timeField = computed(() => {
  if (timeFieldChoice.value === "") return "";
  if (timeFieldChoice.value && (!mappingLoaded.value || dateFields.value.includes(timeFieldChoice.value))) return timeFieldChoice.value;
  return defaultTimeField(mappingFields.value);
});
const timeFieldSelectValue = computed({
  get: () => timeField.value || NO_TIME_FIELD,
  set: (value: string) => {
    timeFieldChoice.value = value === NO_TIME_FIELD ? "" : value;
    void runSearch();
  },
});
const languageSelectValue = computed({
  get: () => language.value,
  set: (value: string) => setLanguage(value as DiscoverQueryLanguage),
});
const sidebarFields = computed(() => withUnmappedFields(mappingFields.value, hits.value));
const activeSort = computed(() => effectiveSort(sort.value, timeField.value || null));
const showHistogram = computed(() => docMode.value && Boolean(timeField.value) && buckets.value.length > 0 && histogramInterval.value !== null);
const canLoadMore = computed(() => docMode.value && lastSearchOptions !== null && hits.value.length > 0 && hits.value.length < MAX_SAMPLE_SIZE && hits.value.length < (total.value?.value ?? 0));
const sampleLimitReached = computed(() => hits.value.length >= MAX_SAMPLE_SIZE && (total.value?.value ?? 0) > hits.value.length);
const queryPlaceholder = computed(() => t(`esDiscover.placeholders.${language.value}`));
const errorMessage = computed(() => (error.value ? [error.value.message, error.value.detail].filter(Boolean).join("\n\n") : ""));
const rangeText = computed(() => (searchedRange.value ? `${formatDateTime(searchedRange.value.from)} → ${formatDateTime(searchedRange.value.to)}` : ""));
const hitCountText = computed(() => {
  if (!total.value) return "";
  const count = total.value.value.toLocaleString();
  return total.value.relation === "gte" ? `≥ ${count}` : count;
});

// ---- state persistence -------------------------------------------------------
const state = computed<DiscoverState>(() => ({
  indexPattern: indexPattern.value,
  timeField: timeFieldChoice.value,
  timeRange: { ...timeRange.value },
  language: language.value,
  query: queryDraft.value,
  filters: cloneFilters(filters.value),
  columns: [...columns.value],
  sort: sort.value.map((entry) => ({ ...entry })),
}));

watch(state, (value) => emit("state-change", JSON.parse(JSON.stringify(value)) as DiscoverState), { deep: true });

// ---- requests -------------------------------------------------------------------
function ensureDistribution(): Promise<ClusterDistribution> {
  distributionPromise ??= detectDistribution(props.connectionId);
  return distributionPromise;
}

async function loadIndices() {
  if (indicesLoading.value || Date.now() - indicesLoadedAt < 30_000) return;
  indicesLoading.value = true;
  try {
    const [indexResponse, aliasResponse] = await Promise.all([discoverRequest(props.connectionId, { method: "GET", path: "/_cat/indices?format=json&h=index" }), discoverRequest(props.connectionId, { method: "GET", path: "/_cat/aliases?format=json&h=alias" }).catch(() => null)]);
    if (isSuccessStatus(indexResponse.status)) {
      const rows = JSON.parse(indexResponse.body) as Array<{ index?: string }>;
      indices.value = markRaw(rows.map((row) => row.index ?? "").filter(Boolean));
    }
    if (aliasResponse && isSuccessStatus(aliasResponse.status)) {
      const rows = JSON.parse(aliasResponse.body) as Array<{ alias?: string }>;
      aliases.value = markRaw([...new Set(rows.map((row) => row.alias ?? "").filter(Boolean))]);
    }
    indicesLoadedAt = Date.now();
  } catch {
    // Suggestions are best-effort; free text still works.
  } finally {
    indicesLoading.value = false;
  }
}

async function loadMapping() {
  const pattern = indexPattern.value.trim();
  const seq = ++mappingSeq;
  if (!pattern) {
    mappingFields.value = [];
    mappingLoaded.value = false;
    return;
  }
  mappingLoading.value = true;
  try {
    const response = await discoverRequest(props.connectionId, { method: "GET", path: `/${encodeIndexPattern(pattern)}/_mapping` });
    if (seq !== mappingSeq || disposed) return;
    mappingFields.value = isSuccessStatus(response.status) ? markRaw(fieldsFromMappingResponse(JSON.parse(response.body))) : [];
  } catch {
    if (seq === mappingSeq) mappingFields.value = [];
  } finally {
    if (seq === mappingSeq) {
      mappingLoaded.value = true;
      mappingLoading.value = false;
    }
  }
}

function resetResults() {
  hits.value = [];
  total.value = null;
  tookMs.value = null;
  buckets.value = [];
  histogramInterval.value = null;
  tabular.value = null;
  searchedRange.value = null;
  lastSearchOptions = null;
}

function browserTimeZone(): string | undefined {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || undefined;
  } catch {
    return undefined;
  }
}

async function runTabularQuery(seq: number, pattern: string) {
  const distribution = await ensureDistribution();
  if (seq !== searchSeq) return;
  if (language.value === "ppl" && distribution === "elasticsearch") {
    error.value = { message: t("esDiscover.pplUnsupported") };
    return;
  }
  const request = language.value === "ppl" ? buildPplRequest(queryDraft.value, pattern) : buildSqlRequest(queryDraft.value, pattern, distribution);
  const response = await discoverRequest(props.connectionId, request);
  if (seq !== searchSeq || disposed) return;
  tookMs.value = response.tookMs;
  if (!isSuccessStatus(response.status)) {
    error.value = extractErrorInfo(response.body, response.status);
    return;
  }
  tabular.value = markRaw(parseTabularResponse(response.body));
}

async function runDocumentSearch(seq: number, pattern: string) {
  const field = timeField.value || null;
  const range = field ? resolveTimeRange(timeRange.value) : null;
  if (field && !range) {
    error.value = { message: t("esDiscover.invalidRange") };
    return;
  }
  const options: SearchBodyOptions = {
    language: language.value,
    query: queryDraft.value,
    filters: cloneFilters(filters.value),
    fields: mappingFields.value,
    timeField: field,
    range,
    sort: activeSort.value,
    size: INITIAL_PAGE_SIZE,
    histogram: true,
    timeZone: browserTimeZone(),
  };
  let built: ReturnType<typeof buildSearchBody>;
  try {
    built = buildSearchBody(options);
  } catch (caught) {
    if (caught instanceof DqlSyntaxError) {
      dqlError.value = caught;
      return;
    }
    throw caught;
  }
  const response = await discoverRequest(props.connectionId, { method: "POST", path: `/${encodeIndexPattern(pattern)}/_search`, body: JSON.stringify(built.body) });
  if (seq !== searchSeq || disposed) return;
  if (!isSuccessStatus(response.status)) {
    error.value = extractErrorInfo(response.body, response.status);
    return;
  }
  const parsed = JSON.parse(response.body) as { took?: number; hits?: { hits?: DiscoverHit[] } };
  hits.value = markRaw(parsed.hits?.hits ?? []);
  total.value = readTotalHits(parsed);
  tookMs.value = typeof parsed.took === "number" ? parsed.took : response.tookMs;
  buckets.value = markRaw(readHistogramBuckets(parsed));
  histogramInterval.value = built.interval ?? null;
  searchedRange.value = range;
  lastSearchOptions = { ...options, histogram: false };
}

/** Run the current query. Out-of-order responses are ignored via a sequence number. */
async function runSearch() {
  const pattern = indexPattern.value.trim();
  const seq = ++searchSeq;
  dqlError.value = null;
  error.value = null;
  loadingMore.value = false;
  if (!pattern) {
    resetResults();
    loading.value = false;
    return;
  }
  loading.value = true;
  try {
    if (docMode.value) {
      tabular.value = null;
      await runDocumentSearch(seq, pattern);
    } else {
      resetResults();
      await runTabularQuery(seq, pattern);
    }
  } catch (caught) {
    if (seq === searchSeq && !disposed) error.value = errorFromUnknown(caught);
  } finally {
    if (seq === searchSeq) {
      loading.value = false;
      hasSearched.value = true;
      if (error.value || dqlError.value) {
        if (docMode.value) {
          hits.value = [];
          total.value = null;
          buckets.value = [];
          lastSearchOptions = null;
        } else {
          tabular.value = null;
        }
      }
    }
  }
}

async function loadMore() {
  const options = lastSearchOptions;
  if (!options || loadingMore.value || !canLoadMore.value) return;
  const seq = searchSeq;
  const pattern = indexPattern.value.trim();
  loadingMore.value = true;
  try {
    const from = hits.value.length;
    const { body } = buildSearchBody({ ...options, from, size: Math.min(INITIAL_PAGE_SIZE, MAX_SAMPLE_SIZE - from) });
    const response = await discoverRequest(props.connectionId, { method: "POST", path: `/${encodeIndexPattern(pattern)}/_search`, body: JSON.stringify(body) });
    if (seq !== searchSeq || disposed) return;
    if (!isSuccessStatus(response.status)) {
      error.value = extractErrorInfo(response.body, response.status);
      return;
    }
    const parsed = JSON.parse(response.body) as { hits?: { hits?: DiscoverHit[] } };
    hits.value = markRaw([...hits.value, ...(parsed.hits?.hits ?? [])]);
  } catch (caught) {
    if (seq === searchSeq) error.value = errorFromUnknown(caught);
  } finally {
    if (seq === searchSeq) loadingMore.value = false;
  }
}

// ---- user actions ------------------------------------------------------------------
async function setIndexPattern(value: string) {
  indexPattern.value = value;
  resetResults();
  await loadMapping();
  await runSearch();
}

function setLanguage(value: DiscoverQueryLanguage) {
  if (value === language.value) return;
  const wasDocMode = docMode.value;
  language.value = value;
  dqlError.value = null;
  error.value = null;
  if (wasDocMode !== docMode.value) {
    resetResults();
    hasSearched.value = false;
  }
}

function submitQuery() {
  void runSearch();
}

function onQueryKeydown(event: KeyboardEvent) {
  if (event.key !== "Enter" || event.isComposing || event.shiftKey) return;
  event.preventDefault();
  submitQuery();
}

function setTimeRange(value: DiscoverTimeRange) {
  timeRange.value = { ...value };
  if (docMode.value) void runSearch();
}

function setFilters(next: DiscoverFilter[]) {
  filters.value = next;
  if (docMode.value) void runSearch();
}

function filterValue(value: unknown): string | number | boolean | null {
  const first = Array.isArray(value) ? value[0] : value;
  if (first === null || first === undefined) return null;
  if (typeof first === "string" || typeof first === "number" || typeof first === "boolean") return first;
  return JSON.stringify(first);
}

function addValueFilter(payload: { field: string; value: unknown; negate: boolean }) {
  const value = filterValue(payload.value);
  if (value === null) {
    // Filtering on a missing value means "field does not exist".
    setFilters([...filters.value, { id: createFilterId(), field: payload.field, operator: payload.negate ? "exists" : "not_exists", enabled: true }]);
    return;
  }
  const operator = payload.negate ? "is_not" : "is";
  const existing = filters.value.find((filter) => filter.field === payload.field && (filter.operator === "is" || filter.operator === "is_not") && filter.value === value);
  if (existing) {
    if (existing.operator === operator && existing.enabled) return;
    setFilters(filters.value.map((filter) => (filter === existing ? { ...filter, operator, enabled: true } : filter)));
    return;
  }
  setFilters([...filters.value, { id: createFilterId(), field: payload.field, operator, value, enabled: true }]);
}

function toggleColumn(field: string) {
  columns.value = columns.value.includes(field) ? columns.value.filter((column) => column !== field) : [...columns.value, field];
}

function onSort(field: string) {
  const current = activeSort.value[0];
  const direction = current?.field === field ? (current.direction === "desc" ? "asc" : "desc") : field === timeField.value ? "desc" : "asc";
  sort.value = [{ field, direction }];
  void runSearch();
}

function onHistogramZoom(range: { from: string; to: string }) {
  setTimeRange(range);
}

onMounted(async () => {
  void ensureDistribution();
  if (indexPattern.value.trim()) {
    await loadMapping();
    await runSearch();
  }
});

onBeforeUnmount(() => {
  disposed = true;
  searchSeq += 1;
  mappingSeq += 1;
});

defineExpose({ runSearch, state });
</script>

<template>
  <div class="flex h-full min-h-0 flex-col bg-background text-foreground" data-testid="elasticsearch-discover">
    <!-- Top bar -->
    <div class="flex shrink-0 flex-wrap items-center gap-2 border-b border-border px-2 py-2">
      <DiscoverIndexPatternInput class="w-64 shrink-0" :model-value="indexPattern" :indices="indices" :aliases="aliases" :loading="indicesLoading || mappingLoading" @open="loadIndices" @update:model-value="setIndexPattern" />
      <Select v-if="docMode" v-model="timeFieldSelectValue">
        <SelectTrigger size="sm" class="h-8 w-40 text-xs" :title="t('esDiscover.timeField')" data-testid="discover-time-field">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem v-for="field in dateFields" :key="field" :value="field" class="font-mono text-xs">{{ field }}</SelectItem>
          <SelectItem :value="NO_TIME_FIELD" class="text-xs">{{ t("esDiscover.noTimeField") }}</SelectItem>
        </SelectContent>
      </Select>
      <Select v-model="languageSelectValue">
        <SelectTrigger size="sm" class="h-8 w-24 text-xs" :title="t('esDiscover.language')" data-testid="discover-language">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem v-for="option in LANGUAGES" :key="option" :value="option" class="text-xs">{{ t(`esDiscover.languages.${option}`) }}</SelectItem>
        </SelectContent>
      </Select>
      <div class="flex min-w-64 flex-1 items-start">
        <textarea
          v-if="!docMode"
          v-model="queryDraft"
          rows="2"
          class="min-h-8 w-full resize-y rounded-md border border-input bg-transparent px-2.5 py-1.5 font-mono text-xs outline-none placeholder:text-muted-foreground focus-visible:border-ring dark:bg-input/30"
          :placeholder="queryPlaceholder"
          spellcheck="false"
          data-testid="discover-query"
          @keydown="onQueryKeydown"
        />
        <input
          v-else
          v-model="queryDraft"
          class="h-8 w-full rounded-md border bg-transparent px-2.5 font-mono text-xs outline-none placeholder:text-muted-foreground focus-visible:border-ring dark:bg-input/30"
          :class="dqlError ? 'border-destructive' : 'border-input'"
          :placeholder="queryPlaceholder"
          :aria-invalid="dqlError ? 'true' : undefined"
          autocapitalize="off"
          autocomplete="off"
          autocorrect="off"
          spellcheck="false"
          data-testid="discover-query"
          @keydown="onQueryKeydown"
        />
      </div>
      <DiscoverTimeRangePicker :model-value="timeRange" :disabled="!docMode || !timeField" @update:model-value="setTimeRange" />
      <Button size="sm" class="h-8" :disabled="!indexPattern.trim()" data-testid="discover-refresh" @click="submitQuery">
        <LoaderCircle v-if="loading" class="size-3.5 animate-spin" />
        <RefreshCw v-else class="size-3.5" />
        {{ t("esDiscover.refresh") }}
      </Button>
    </div>

    <div v-if="dqlError" class="shrink-0 border-b border-border bg-destructive/10 px-3 py-1.5 text-xs text-destructive" data-testid="discover-dql-error">
      <div class="font-medium">{{ t("esDiscover.dqlSyntaxError") }}: {{ dqlError.message }}</div>
      <pre class="mt-1 overflow-x-auto font-mono text-[11px] leading-4">{{ formatDqlErrorPointer(dqlError) }}</pre>
    </div>

    <div v-if="docMode" class="shrink-0 border-b border-border px-2 py-1">
      <DiscoverFilterBar :filters="filters" :fields="mappingFields" @update:filters="setFilters" />
    </div>

    <div class="flex min-h-0 flex-1">
      <aside v-if="docMode" class="w-60 shrink-0 border-r border-border">
        <DiscoverFieldSidebar :fields="sidebarFields" :columns="columns" :hits="hits" :can-filter="docMode" @toggle-column="toggleColumn" @add-filter="addValueFilter" />
      </aside>

      <main class="flex min-w-0 flex-1 flex-col">
        <div class="flex h-8 shrink-0 items-center gap-3 border-b border-border px-3 text-xs">
          <template v-if="docMode && total">
            <span data-testid="discover-hit-count"
              ><span class="font-semibold">{{ hitCountText }}</span> {{ t("esDiscover.hits") }}</span
            >
            <span v-if="rangeText" class="truncate text-muted-foreground">{{ rangeText }}</span>
          </template>
          <span v-else-if="!docMode && tabular" class="font-semibold">{{ t("esDiscover.rows", { count: tabular.rows.length }) }}</span>
          <span v-if="tookMs !== null" class="text-muted-foreground" data-testid="discover-took">{{ t("esDiscover.took", { ms: tookMs }) }}</span>
          <LoaderCircle v-if="loading" class="ml-auto size-3.5 animate-spin text-muted-foreground" />
        </div>

        <div v-if="showHistogram && histogramInterval" class="h-36 shrink-0 border-b border-border px-2 py-1">
          <DiscoverHistogram :buckets="buckets" :interval-ms="histogramInterval.ms" :interval-label="histogramInterval.expression" @zoom="onHistogramZoom" />
        </div>

        <div class="relative min-h-0 flex-1">
          <div v-if="!indexPattern.trim()" class="flex h-full flex-col items-center justify-center gap-2 p-6 text-center text-sm text-muted-foreground">
            {{ t("esDiscover.enterIndexPattern") }}
          </div>
          <div v-else-if="error" class="h-full overflow-auto p-4" data-testid="discover-error">
            <ErrorBanner variant="card" :title="t('esDiscover.requestFailed')" :message="errorMessage" />
          </div>
          <div v-else-if="loading && hits.length === 0 && !tabular" class="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
            <LoaderCircle class="size-4 animate-spin" />
            {{ t("esDiscover.loading") }}
          </div>
          <template v-else-if="docMode">
            <DiscoverDocTable
              v-if="hits.length > 0"
              :hits="hits"
              :columns="columns"
              :time-field="timeField"
              :fields="sidebarFields"
              :sort="activeSort"
              :can-filter="docMode"
              :can-load-more="canLoadMore"
              :loading-more="loadingMore"
              :sample-limit-reached="sampleLimitReached"
              @sort="onSort"
              @toggle-column="toggleColumn"
              @add-filter="addValueFilter"
              @load-more="loadMore"
            />
            <div v-else-if="hasSearched && !dqlError" class="flex h-full flex-col items-center justify-center gap-2 p-6 text-center text-sm text-muted-foreground" data-testid="discover-empty">
              <SearchX class="size-8 opacity-40" />
              <div class="font-medium text-foreground">{{ t("esDiscover.noResults") }}</div>
              <div class="text-xs">{{ timeField ? t("esDiscover.noResultsHintTime") : t("esDiscover.noResultsHint") }}</div>
            </div>
          </template>
          <template v-else>
            <DiscoverTabularResult v-if="tabular" :result="tabular" />
            <div v-else class="flex h-full items-center justify-center p-6 text-center text-sm text-muted-foreground">{{ t("esDiscover.runHint") }}</div>
          </template>
        </div>
      </main>
    </div>
  </div>
</template>
