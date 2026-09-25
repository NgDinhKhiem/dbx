<script setup lang="ts">
import { computed } from "vue";
import { Braces, Calendar, CircleHelp, Hash, MapPin, Network, ToggleLeft, TriangleAlert, Type, Binary, KeyRound } from "@lucide/vue";
import type { DiscoverFieldType } from "@/lib/elasticsearch/discover/types";

const props = defineProps<{ type: DiscoverFieldType }>();

const ICONS = {
  string: Type,
  text: Type,
  keyword: KeyRound,
  number: Hash,
  date: Calendar,
  boolean: ToggleLeft,
  ip: Network,
  geo: MapPin,
  object: Braces,
  nested: Braces,
  binary: Binary,
  conflict: TriangleAlert,
  unknown: CircleHelp,
} as const;

const COLORS: Partial<Record<DiscoverFieldType, string>> = {
  text: "text-sky-600 dark:text-sky-400",
  string: "text-sky-600 dark:text-sky-400",
  keyword: "text-teal-600 dark:text-teal-400",
  number: "text-violet-600 dark:text-violet-400",
  date: "text-amber-600 dark:text-amber-400",
  boolean: "text-rose-600 dark:text-rose-400",
  conflict: "text-destructive",
};

const icon = computed(() => ICONS[props.type] ?? CircleHelp);
const color = computed(() => COLORS[props.type] ?? "text-muted-foreground");
</script>

<template>
  <component :is="icon" class="size-3.5 shrink-0" :class="color" :data-field-type="type" aria-hidden="true" />
</template>
