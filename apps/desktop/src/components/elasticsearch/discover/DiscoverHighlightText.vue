<script setup lang="ts">
import type { HighlightSegment } from "@/lib/elasticsearch/discover/types";

// Renders highlighter output as text nodes only: document data never reaches v-html.
defineProps<{ segments: readonly HighlightSegment[] }>();
</script>

<template>
  <span class="break-words"
    ><template v-for="(segment, index) in segments" :key="index"
      ><mark v-if="segment.highlighted" class="rounded-sm bg-yellow-300/70 px-0.5 text-foreground dark:bg-yellow-500/40">{{ segment.text }}</mark
      ><template v-else>{{ segment.text }}</template></template
    ></span
  >
</template>
