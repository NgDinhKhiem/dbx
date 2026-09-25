<script setup lang="ts">
import { computed, shallowRef, watch } from "vue";
import { useI18n } from "vue-i18n";
import { use } from "echarts/core";
import { CanvasRenderer } from "echarts/renderers";
import { BarChart } from "echarts/charts";
import { BrushComponent, GridComponent, TooltipComponent } from "echarts/components";
import VChart from "vue-echarts";
import { useTheme } from "@/composables/useTheme";
import { formatBucketLabel, formatDateTime } from "@/lib/elasticsearch/discover/timeRange";
import type { HistogramBucket } from "@/lib/elasticsearch/discover/requestBuilder";

use([CanvasRenderer, BarChart, GridComponent, TooltipComponent, BrushComponent]);

const props = defineProps<{
  buckets: readonly HistogramBucket[];
  intervalMs: number;
  intervalLabel: string;
}>();

const emit = defineEmits<{ (e: "zoom", range: { from: string; to: string }): void }>();

const { t } = useI18n();
const { isDark } = useTheme();
const chart = shallowRef<InstanceType<typeof VChart> | null>(null);

const option = computed(() => {
  const axisColor = isDark.value ? "#a1a1aa" : "#52525b";
  const splitColor = isDark.value ? "rgba(255,255,255,0.06)" : "rgba(0,0,0,0.06)";
  return {
    animation: false,
    grid: { left: 48, right: 12, top: 8, bottom: 24 },
    tooltip: {
      trigger: "axis",
      axisPointer: { type: "shadow" },
      formatter: (params: Array<{ dataIndex: number; value: number }>) => {
        const first = params[0];
        const bucket = first ? props.buckets[first.dataIndex] : undefined;
        if (!bucket) return "";
        // Plain text only (tooltip content is built from numbers and dates, never document data).
        return `${formatDateTime(new Date(bucket.key), false)}<br/>${t("esDiscover.count")}: ${bucket.count}`;
      },
    },
    brush: { toolbox: [], xAxisIndex: 0, brushType: "lineX", brushMode: "single", throttleType: "debounce", transformable: false },
    xAxis: {
      type: "category",
      data: props.buckets.map((bucket) => formatBucketLabel(bucket.key, props.intervalMs)),
      axisLabel: { color: axisColor, fontSize: 10, hideOverlap: true },
      axisLine: { lineStyle: { color: splitColor } },
      axisTick: { show: false },
    },
    yAxis: {
      type: "value",
      minInterval: 1,
      axisLabel: { color: axisColor, fontSize: 10 },
      splitLine: { lineStyle: { color: splitColor } },
    },
    series: [
      {
        type: "bar",
        data: props.buckets.map((bucket) => bucket.count),
        barCategoryGap: "15%",
        itemStyle: { color: isDark.value ? "#60a5fa" : "#3b82f6" },
        emphasis: { itemStyle: { color: isDark.value ? "#93c5fd" : "#1d4ed8" } },
      },
    ],
  };
});

function zoomToIndexes(startIndex: number, endIndex: number) {
  const start = props.buckets[Math.max(0, Math.min(startIndex, endIndex))];
  const end = props.buckets[Math.min(props.buckets.length - 1, Math.max(startIndex, endIndex))];
  if (!start || !end) return;
  emit("zoom", { from: new Date(start.key).toISOString(), to: new Date(end.key + props.intervalMs - 1).toISOString() });
}

function onClick(params: { dataIndex?: number }) {
  if (typeof params.dataIndex === "number") zoomToIndexes(params.dataIndex, params.dataIndex);
}

// Arm the lineX brush once per option update so dragging across bars zooms.
let brushArmed = false;
watch(option, () => {
  brushArmed = false;
});

function onChartFinished() {
  if (brushArmed) return;
  brushArmed = true;
  chart.value?.dispatchAction({ type: "takeGlobalCursor", key: "brush", brushOption: { brushType: "lineX", brushMode: "single" } });
}

function onBrushEnd(params: { areas?: Array<{ coordRange?: [number, number] }> }) {
  const range = params.areas?.[0]?.coordRange;
  chart.value?.dispatchAction({ type: "brush", areas: [] });
  if (!range || range.length < 2) return;
  zoomToIndexes(Math.round(range[0]), Math.round(range[1]));
}
</script>

<template>
  <div class="relative h-full w-full" data-testid="discover-histogram">
    <VChart ref="chart" :option="option" autoresize class="h-full w-full" @click="onClick" @brush-end="onBrushEnd" @finished="onChartFinished" />
    <div class="pointer-events-none absolute top-0 right-3 text-[10px] text-muted-foreground">{{ t("esDiscover.interval", { interval: intervalLabel }) }}</div>
  </div>
</template>
