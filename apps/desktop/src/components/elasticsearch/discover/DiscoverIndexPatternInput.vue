<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { Database, LayoutDashboard, LoaderCircle } from "@lucide/vue";
import { indexPatternSuggestions, type IndexSuggestion } from "@/lib/elasticsearch/discover/indexPatterns";

const props = defineProps<{
  modelValue: string;
  indices: readonly string[];
  aliases: readonly string[];
  /** Index patterns saved in OpenSearch Dashboards / Kibana. */
  dashboardsPatterns?: readonly { title: string; timeField?: string }[];
  loading?: boolean;
}>();

const emit = defineEmits<{
  (e: "update:modelValue", value: string): void;
  (e: "open"): void;
}>();

const { t } = useI18n();
const draft = ref(props.modelValue);
const open = ref(false);
const activeIndex = ref(-1);

watch(
  () => props.modelValue,
  (value) => {
    draft.value = value;
  },
);

const suggestions = computed<IndexSuggestion[]>(() => (open.value ? indexPatternSuggestions({ indices: props.indices, aliases: props.aliases, dashboardsPatterns: props.dashboardsPatterns, typed: draft.value === props.modelValue ? "" : draft.value, limit: 60 }) : []));

function showList() {
  if (!open.value) emit("open");
  open.value = true;
  activeIndex.value = -1;
}

function commit(value: string) {
  const next = value.trim();
  draft.value = next;
  open.value = false;
  activeIndex.value = -1;
  if (next && next !== props.modelValue) emit("update:modelValue", next);
}

function onKeydown(event: KeyboardEvent) {
  if (event.isComposing) return;
  if (event.key === "ArrowDown") {
    event.preventDefault();
    if (!open.value) showList();
    activeIndex.value = Math.min(activeIndex.value + 1, suggestions.value.length - 1);
  } else if (event.key === "ArrowUp") {
    event.preventDefault();
    activeIndex.value = Math.max(activeIndex.value - 1, -1);
  } else if (event.key === "Enter") {
    event.preventDefault();
    const active = suggestions.value[activeIndex.value];
    commit(active ? active.value : draft.value);
  } else if (event.key === "Escape") {
    open.value = false;
    draft.value = props.modelValue;
  }
}

function onBlur() {
  // Let a mousedown on a suggestion win over the blur.
  window.setTimeout(() => {
    if (open.value) commit(draft.value || props.modelValue);
  }, 120);
}
</script>

<template>
  <div class="relative min-w-0">
    <div class="flex h-8 items-center gap-1.5 rounded-md border border-input bg-transparent px-2 focus-within:border-ring dark:bg-input/30">
      <Database class="size-3.5 shrink-0 text-muted-foreground" />
      <input
        v-model="draft"
        data-testid="discover-index-pattern"
        class="h-full min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground"
        :placeholder="t('esDiscover.indexPatternPlaceholder')"
        :aria-label="t('esDiscover.indexPattern')"
        autocapitalize="off"
        autocomplete="off"
        autocorrect="off"
        spellcheck="false"
        role="combobox"
        :aria-expanded="open"
        @focus="showList"
        @input="showList"
        @keydown="onKeydown"
        @blur="onBlur"
      />
      <LoaderCircle v-if="loading" class="size-3.5 shrink-0 animate-spin text-muted-foreground" />
    </div>
    <div v-if="open && (suggestions.length > 0 || loading)" class="absolute top-full left-0 z-50 mt-1 max-h-80 w-full min-w-72 overflow-y-auto rounded-md border border-border bg-popover p-1 text-popover-foreground shadow-md" role="listbox">
      <div v-if="loading && suggestions.length === 0" class="px-2 py-1.5 text-xs text-muted-foreground">{{ t("esDiscover.loadingIndices") }}</div>
      <button
        v-for="(item, index) in suggestions"
        :key="`${item.kind}:${item.value}`"
        type="button"
        role="option"
        :aria-selected="index === activeIndex"
        class="flex w-full items-center gap-2 rounded-sm px-2 py-1 text-left text-xs hover:bg-muted"
        :class="index === activeIndex ? 'bg-muted' : ''"
        @mousedown.prevent="commit(item.value)"
      >
        <LayoutDashboard v-if="item.kind === 'dashboards'" class="size-3 shrink-0 text-primary" />
        <span class="min-w-0 flex-1 truncate font-mono">{{ item.value }}</span>
        <span class="shrink-0 text-[10px] text-muted-foreground" :data-testid="item.kind === 'dashboards' ? 'discover-dashboards-pattern' : undefined">
          <template v-if="item.kind === 'dashboards'">{{ item.timeField ? t("esDiscover.dashboardsPatternTime", { field: item.timeField }) : t("esDiscover.dashboardsPattern") }}</template>
          <template v-else>{{ item.kind === "pattern" ? t("esDiscover.patternMatches", { count: item.matches ?? 0 }) : item.kind === "alias" ? t("esDiscover.alias") : t("esDiscover.index") }}</template>
        </span>
      </button>
    </div>
  </div>
</template>
