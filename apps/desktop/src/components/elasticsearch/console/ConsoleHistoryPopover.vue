<script setup lang="ts">
import { ref } from "vue";
import { useI18n } from "vue-i18n";
import { History, Trash2 } from "@lucide/vue";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import type { ConsoleHistoryEntry } from "@/lib/elasticsearch/console/consoleHistory";

defineProps<{ entries: readonly ConsoleHistoryEntry[] }>();

const emit = defineEmits<{
  insert: [entry: ConsoleHistoryEntry];
  clear: [];
}>();

const { t } = useI18n();
const open = ref(false);

function formatTime(executedAt: number): string {
  return new Date(executedAt).toLocaleString();
}

function pick(entry: ConsoleHistoryEntry) {
  open.value = false;
  emit("insert", entry);
}
</script>

<template>
  <Popover v-model:open="open">
    <PopoverTrigger as-child>
      <Button variant="ghost" size="sm" class="h-7 gap-1 px-2 text-xs" :title="t('esConsole.history')" data-testid="es-console-history-trigger">
        <History class="h-3.5 w-3.5" />
        {{ t("esConsole.history") }}
      </Button>
    </PopoverTrigger>
    <PopoverContent align="start" class="w-96 max-w-[90vw] gap-1.5 p-1.5">
      <div class="flex items-center justify-between px-1.5 pt-0.5">
        <span class="text-xs font-medium text-muted-foreground">{{ t("esConsole.historyTitle", { count: entries.length }) }}</span>
        <Button v-if="entries.length" variant="ghost" size="icon-xs" class="text-muted-foreground" :title="t('esConsole.historyClear')" :aria-label="t('esConsole.historyClear')" @click="emit('clear')">
          <Trash2 class="h-3.5 w-3.5" />
        </Button>
      </div>
      <p v-if="!entries.length" class="px-1.5 py-3 text-center text-xs text-muted-foreground">{{ t("esConsole.historyEmpty") }}</p>
      <ul v-else class="max-h-80 overflow-y-auto" role="list">
        <li v-for="entry in entries" :key="`${entry.executedAt}:${entry.label}`">
          <button type="button" class="flex w-full min-w-0 flex-col items-start gap-0.5 rounded px-1.5 py-1 text-left hover:bg-muted focus-visible:bg-muted focus-visible:outline-none" :title="t('esConsole.historyInsert')" @click="pick(entry)">
            <span class="w-full truncate font-mono text-xs text-foreground">{{ entry.label }}</span>
            <span class="text-[11px] text-muted-foreground">
              {{ formatTime(entry.executedAt) }}
              <template v-if="entry.truncated"> · {{ t("esConsole.historyTruncated") }}</template>
            </span>
          </button>
        </li>
      </ul>
    </PopoverContent>
  </Popover>
</template>
