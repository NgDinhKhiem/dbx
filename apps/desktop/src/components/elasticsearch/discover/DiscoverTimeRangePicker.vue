<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { Clock } from "@lucide/vue";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { formatDateTime, isRelativeDate, parseDateMath, QUICK_RANGES, quickRangeFor } from "@/lib/elasticsearch/discover/timeRange";
import type { DiscoverTimeRange } from "@/lib/elasticsearch/discover/types";

const props = defineProps<{ modelValue: DiscoverTimeRange; disabled?: boolean }>();
const emit = defineEmits<{ (e: "update:modelValue", value: DiscoverTimeRange): void }>();

const { t } = useI18n();
const open = ref(false);
const fromDraft = ref("");
const toDraft = ref("");
const invalid = ref(false);

function displayDate(expression: string): string {
  if (isRelativeDate(expression)) return expression;
  const date = parseDateMath(expression);
  return date ? formatDateTime(date, false) : expression;
}

watch(
  [open, () => props.modelValue],
  () => {
    fromDraft.value = displayDate(props.modelValue.from);
    toDraft.value = displayDate(props.modelValue.to);
    invalid.value = false;
  },
  { immediate: true },
);

const label = computed(() => {
  const quick = quickRangeFor(props.modelValue);
  if (quick) return t(`esDiscover.quickRanges.${quick.key}`);
  return `${displayDate(props.modelValue.from)} → ${displayDate(props.modelValue.to)}`;
});

function pickQuick(from: string, to: string) {
  emit("update:modelValue", { from, to });
  open.value = false;
}

/** Absolute local "YYYY-MM-DD HH:mm:ss" input -> ISO; date math is kept as typed. */
function normalizeInput(value: string): string | null {
  const text = value.trim();
  if (isRelativeDate(text)) return parseDateMath(text) ? text : null;
  const date = parseDateMath(text.replace(" ", "T"));
  return date ? date.toISOString() : null;
}

function applyAbsolute() {
  const from = normalizeInput(fromDraft.value);
  const to = normalizeInput(toDraft.value);
  const fromDate = from ? parseDateMath(from) : null;
  const toDate = to ? parseDateMath(to, new Date(), true) : null;
  if (!from || !to || !fromDate || !toDate || fromDate.getTime() > toDate.getTime()) {
    invalid.value = true;
    return;
  }
  emit("update:modelValue", { from, to });
  open.value = false;
}
</script>

<template>
  <Popover v-model:open="open">
    <PopoverTrigger as-child>
      <Button variant="outline" size="sm" class="h-8 max-w-80 gap-1.5 px-2 text-xs" :disabled="disabled" data-testid="discover-time-range" :title="t('esDiscover.timeRange')">
        <Clock class="size-3.5 text-muted-foreground" />
        <span class="truncate">{{ label }}</span>
      </Button>
    </PopoverTrigger>
    <PopoverContent align="end" class="w-80 gap-3 p-3">
      <div>
        <div class="mb-1.5 text-xs font-medium text-muted-foreground">{{ t("esDiscover.commonlyUsed") }}</div>
        <div class="grid grid-cols-2 gap-1">
          <button
            v-for="range in QUICK_RANGES"
            :key="range.key"
            type="button"
            class="rounded-sm px-2 py-1 text-left text-xs hover:bg-muted"
            :class="modelValue.from === range.from && modelValue.to === range.to ? 'bg-primary/10 text-primary' : ''"
            :data-testid="`discover-quick-${range.key}`"
            @click="pickQuick(range.from, range.to)"
          >
            {{ t(`esDiscover.quickRanges.${range.key}`) }}
          </button>
        </div>
      </div>
      <div class="flex flex-col gap-1.5 border-t border-border pt-2">
        <div class="text-xs font-medium text-muted-foreground">{{ t("esDiscover.absoluteRange") }}</div>
        <label class="flex items-center gap-2 text-xs">
          <span class="w-10 shrink-0 text-muted-foreground">{{ t("esDiscover.start") }}</span>
          <Input v-model="fromDraft" class="h-7 font-mono text-xs" placeholder="2026-09-25 00:00:00 / now-1h" @keydown.enter.prevent="applyAbsolute" />
        </label>
        <label class="flex items-center gap-2 text-xs">
          <span class="w-10 shrink-0 text-muted-foreground">{{ t("esDiscover.end") }}</span>
          <Input v-model="toDraft" class="h-7 font-mono text-xs" placeholder="now" @keydown.enter.prevent="applyAbsolute" />
        </label>
        <div class="flex items-center justify-between gap-2">
          <span class="text-[11px]" :class="invalid ? 'text-destructive' : 'text-muted-foreground'">{{ invalid ? t("esDiscover.invalidRange") : t("esDiscover.rangeHint") }}</span>
          <Button size="xs" @click="applyAbsolute">{{ t("esDiscover.apply") }}</Button>
        </div>
      </div>
    </PopoverContent>
  </Popover>
</template>
