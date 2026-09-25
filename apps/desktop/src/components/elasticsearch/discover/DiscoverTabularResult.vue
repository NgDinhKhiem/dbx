<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { formatFieldValue } from "@/lib/elasticsearch/discover/documents";
import type { TabularResult } from "@/lib/elasticsearch/discover/types";

const RENDER_STEP = 200;

const props = defineProps<{ result: TabularResult }>();
const { t } = useI18n();
const renderLimit = ref(RENDER_STEP);

watch(
  () => props.result,
  () => {
    renderLimit.value = RENDER_STEP;
  },
);

const visibleRows = computed(() => props.result.rows.slice(0, renderLimit.value));

function cellText(value: unknown): string {
  return value === undefined ? "" : formatFieldValue(value);
}

function onScroll(event: Event) {
  const target = event.target as HTMLElement;
  if (renderLimit.value < props.result.rows.length && target.scrollTop + target.clientHeight >= target.scrollHeight - 400) {
    renderLimit.value = Math.min(props.result.rows.length, renderLimit.value + RENDER_STEP);
  }
}
</script>

<template>
  <div class="h-full min-h-0 overflow-auto" data-testid="discover-tabular-result" @scroll.passive="onScroll">
    <table class="min-w-full border-separate border-spacing-0 text-xs">
      <thead class="sticky top-0 z-10 bg-background">
        <tr>
          <th class="border-b border-border px-2 py-1.5 text-right font-medium text-muted-foreground">#</th>
          <th v-for="(column, index) in result.columns" :key="index" class="border-b border-border px-2 py-1.5 text-left font-medium whitespace-nowrap">
            <span class="font-mono">{{ column.name }}</span>
            <span v-if="column.type" class="ml-1 text-[10px] font-normal text-muted-foreground">{{ column.type }}</span>
          </th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="(row, rowIndex) in visibleRows" :key="rowIndex" class="hover:bg-muted/40">
          <td class="border-b border-border/60 px-2 py-1 text-right text-muted-foreground tabular-nums">{{ rowIndex + 1 }}</td>
          <td v-for="(_, columnIndex) in result.columns" :key="columnIndex" class="max-w-[32rem] truncate border-b border-border/60 px-2 py-1 font-mono" :title="cellText(row[columnIndex])">
            <span v-if="row[columnIndex] === null || row[columnIndex] === undefined" class="text-muted-foreground italic">null</span>
            <template v-else>{{ cellText(row[columnIndex]) }}</template>
          </td>
        </tr>
      </tbody>
    </table>
    <div v-if="result.rows.length === 0" class="py-8 text-center text-xs text-muted-foreground">{{ t("esDiscover.noRows") }}</div>
  </div>
</template>
