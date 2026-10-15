<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { Plus, X } from "@lucide/vue";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { SearchableSelect } from "@/components/ui/searchable-select";
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import { createFilterId, describeFilter, toggleFilterNegation } from "@/lib/elasticsearch/discover/requestBuilder";
import type { DiscoverField, DiscoverFilter, DiscoverFilterOperator } from "@/lib/elasticsearch/discover/types";

const props = defineProps<{
  filters: readonly DiscoverFilter[];
  fields: readonly DiscoverField[];
  disabled?: boolean;
}>();

const emit = defineEmits<{ (e: "update:filters", value: DiscoverFilter[]): void }>();

const { t } = useI18n();

const OPERATORS: DiscoverFilterOperator[] = ["is", "is_not", "exists", "not_exists", "between"];

const adding = ref(false);
const newField = ref("");
const newOperator = ref<DiscoverFilterOperator>("is");
const newValue = ref("");
const newFrom = ref("");
const newTo = ref("");

const fieldOptions = computed(() => props.fields.filter((field) => field.searchable || field.aggregatable).map((field) => field.name));
const needsValue = computed(() => newOperator.value === "is" || newOperator.value === "is_not");
const canAdd = computed(() => {
  if (!newField.value) return false;
  if (needsValue.value) return newValue.value.trim() !== "";
  if (newOperator.value === "between") return newFrom.value.trim() !== "" || newTo.value.trim() !== "";
  return true;
});

function resetDraft() {
  newField.value = "";
  newOperator.value = "is";
  newValue.value = "";
  newFrom.value = "";
  newTo.value = "";
}

function coerceValue(fieldName: string, raw: string): string | number | boolean {
  const field = props.fields.find((candidate) => candidate.name === fieldName);
  const text = raw.trim();
  if (field?.type === "number" && text !== "" && Number.isFinite(Number(text))) return Number(text);
  if (field?.type === "boolean" && (text === "true" || text === "false")) return text === "true";
  return text;
}

function addFilter() {
  if (!canAdd.value) return;
  const filter: DiscoverFilter = { id: createFilterId(), field: newField.value, operator: newOperator.value, enabled: true };
  if (needsValue.value) filter.value = coerceValue(newField.value, newValue.value);
  if (newOperator.value === "between") filter.range = { gte: newFrom.value.trim() || undefined, lt: newTo.value.trim() || undefined };
  emit("update:filters", [...props.filters, filter]);
  adding.value = false;
  resetDraft();
}

function update(id: string, change: (filter: DiscoverFilter) => DiscoverFilter | null) {
  const next: DiscoverFilter[] = [];
  for (const filter of props.filters) {
    if (filter.id !== id) {
      next.push(filter);
      continue;
    }
    const changed = change(filter);
    if (changed) next.push(changed);
  }
  emit("update:filters", next);
}

function setAllEnabled(enabled: boolean) {
  emit(
    "update:filters",
    props.filters.map((filter) => ({ ...filter, enabled })),
  );
}
</script>

<template>
  <div class="flex min-h-8 flex-wrap items-center gap-1.5 text-xs" :class="disabled ? 'opacity-60' : ''" data-testid="discover-filter-bar">
    <DropdownMenu v-for="filter in filters" :key="filter.id">
      <DropdownMenuTrigger as-child>
        <button
          type="button"
          class="group inline-flex h-6 max-w-80 items-center gap-1 rounded-full border px-2 font-mono"
          :class="[describeFilter(filter).negated ? 'border-destructive/40 bg-destructive/10' : 'border-primary/30 bg-primary/10', filter.enabled ? '' : 'line-through opacity-50']"
          :data-testid="`discover-filter-pill-${filter.field}`"
        >
          <span v-if="describeFilter(filter).negated" class="font-sans font-semibold text-destructive">{{ t("esDiscover.not") }}</span>
          <span class="truncate">{{ describeFilter(filter).field }}:</span>
          <span class="truncate font-semibold">{{ describeFilter(filter).value }}</span>
          <X class="size-3 shrink-0 opacity-60 hover:opacity-100" :aria-label="t('esDiscover.removeFilter')" @click.stop="update(filter.id, () => null)" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" class="w-52 text-xs">
        <DropdownMenuItem v-if="filter.operator !== 'between'" @select="update(filter.id, toggleFilterNegation)">
          {{ describeFilter(filter).negated ? t("esDiscover.includeResults") : t("esDiscover.excludeResults") }}
        </DropdownMenuItem>
        <DropdownMenuItem @select="update(filter.id, (current) => ({ ...current, enabled: !current.enabled }))">
          {{ filter.enabled ? t("esDiscover.disableFilter") : t("esDiscover.enableFilter") }}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem variant="destructive" @select="update(filter.id, () => null)">{{ t("esDiscover.removeFilter") }}</DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>

    <Popover v-model:open="adding">
      <PopoverTrigger as-child>
        <Button variant="ghost" size="xs" class="h-6 text-xs text-primary" data-testid="discover-add-filter">
          <Plus class="size-3" />
          {{ t("esDiscover.addFilter") }}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" class="w-96 gap-2 p-3 text-xs">
        <div class="font-medium">{{ t("esDiscover.addFilter") }}</div>
        <div class="grid grid-cols-[1fr_9rem] gap-2">
          <SearchableSelect v-model="newField" :options="fieldOptions" :placeholder="t('esDiscover.selectField')" :search-placeholder="t('esDiscover.searchFields')" :empty-text="t('esDiscover.noFields')" trigger-class="h-8 w-full justify-between text-xs" />
          <Select v-model="newOperator">
            <SelectTrigger class="h-8 w-full text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem v-for="operator in OPERATORS" :key="operator" :value="operator" class="text-xs">{{ t(`esDiscover.operators.${operator}`) }}</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <Input v-if="needsValue" v-model="newValue" class="h-8 text-xs" :placeholder="t('esDiscover.value')" @keydown.enter.prevent="addFilter" />
        <div v-else-if="newOperator === 'between'" class="grid grid-cols-2 gap-2">
          <Input v-model="newFrom" class="h-8 text-xs" :placeholder="t('esDiscover.rangeStart')" @keydown.enter.prevent="addFilter" />
          <Input v-model="newTo" class="h-8 text-xs" :placeholder="t('esDiscover.rangeEnd')" @keydown.enter.prevent="addFilter" />
        </div>
        <div class="flex justify-end gap-2">
          <Button variant="ghost" size="xs" @click="adding = false">{{ t("esDiscover.cancel") }}</Button>
          <Button size="xs" :disabled="!canAdd" @click="addFilter">{{ t("esDiscover.save") }}</Button>
        </div>
      </PopoverContent>
    </Popover>

    <template v-if="filters.length > 1">
      <button type="button" class="text-muted-foreground hover:text-foreground" @click="setAllEnabled(!filters.every((filter) => filter.enabled))">
        {{ filters.every((filter) => filter.enabled) ? t("esDiscover.disableAll") : t("esDiscover.enableAll") }}
      </button>
      <button type="button" class="text-muted-foreground hover:text-destructive" @click="emit('update:filters', [])">{{ t("esDiscover.removeAll") }}</button>
    </template>
    <span v-if="disabled && filters.length > 0" class="text-muted-foreground">{{ t("esDiscover.filtersNotApplied") }}</span>
  </div>
</template>
