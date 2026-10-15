<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";
import { CircleHelp } from "@lucide/vue";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";

const props = defineProps<{ shortcutMod: string }>();

const { t } = useI18n();

const shortcuts = computed(() => [
  { keys: `${props.shortcutMod}+Enter`, label: t("esConsole.shortcutRun") },
  { keys: `${props.shortcutMod}+I`, label: t("esConsole.shortcutIndent") },
  { keys: `${props.shortcutMod}+/`, label: t("esConsole.shortcutComment") },
  { keys: "Ctrl+Space", label: t("esConsole.shortcutComplete") },
  { keys: `${props.shortcutMod}+Z`, label: t("esConsole.shortcutUndo") },
]);
</script>

<template>
  <Popover>
    <PopoverTrigger as-child>
      <Button variant="ghost" size="sm" class="h-7 gap-1 px-2 text-xs" :title="t('esConsole.help')" data-testid="es-console-help-trigger">
        <CircleHelp class="h-3.5 w-3.5" />
        {{ t("esConsole.help") }}
      </Button>
    </PopoverTrigger>
    <PopoverContent align="start" class="w-80 max-w-[90vw] gap-2 text-xs">
      <p class="font-medium text-foreground">{{ t("esConsole.helpTitle") }}</p>
      <dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1.5">
        <template v-for="shortcut in shortcuts" :key="shortcut.keys">
          <dt>
            <kbd class="rounded border border-border bg-muted px-1.5 py-0.5 font-mono text-[11px] text-foreground">{{ shortcut.keys }}</kbd>
          </dt>
          <dd class="text-muted-foreground">{{ shortcut.label }}</dd>
        </template>
      </dl>
      <p class="border-t border-border pt-2 leading-5 text-muted-foreground">{{ t("esConsole.helpSyntax") }}</p>
    </PopoverContent>
  </Popover>
</template>
