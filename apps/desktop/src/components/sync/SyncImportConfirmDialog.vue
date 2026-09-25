<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";
import { AlertTriangle, ArrowRight, ShieldAlert } from "@lucide/vue";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import type { SyncImportReview } from "@/lib/backend/api";

const { t } = useI18n();

const open = defineModel<boolean>("open", { default: false });

const props = defineProps<{
  review: SyncImportReview | null;
}>();

const emit = defineEmits<{
  /** keepLocalSecrets: true keeps local saved credentials of changed connections, false deletes them. */
  confirm: [keepLocalSecrets: boolean];
  cancel: [];
}>();

const endpointChanges = computed(() => props.review?.endpointChanges ?? []);
const unauthenticated = computed(() => !!props.review && !props.review.authenticated);

function displayValue(value: string): string {
  return value.trim() ? value : "—";
}

function choose(keepLocalSecrets: boolean) {
  emit("confirm", keepLocalSecrets);
  open.value = false;
}

function onOpenChange(value: boolean) {
  if (value) return;
  // Closing without a choice (Esc, overlay, close button) cancels the import.
  if (open.value) emit("cancel");
  open.value = false;
}
</script>

<template>
  <Dialog :open="open" @update:open="onOpenChange">
    <DialogContent class="max-w-lg" data-sync-import-confirm-dialog>
      <DialogHeader>
        <DialogTitle class="flex items-center gap-2">
          <ShieldAlert class="h-5 w-5 text-amber-500" aria-hidden="true" />
          {{ t("settings.syncImportConfirmTitle") }}
        </DialogTitle>
        <DialogDescription>{{ t("settings.syncImportConfirmDescription") }}</DialogDescription>
      </DialogHeader>

      <div class="space-y-3 text-sm">
        <div v-if="unauthenticated" data-sync-import-unauthenticated class="flex gap-2 rounded-md border border-red-500/30 bg-red-500/5 px-3 py-2 text-xs text-red-600 dark:text-red-400" role="alert">
          <AlertTriangle class="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
          <span>{{ t("settings.syncImportUnauthenticated") }}</span>
        </div>

        <div v-if="endpointChanges.length" class="space-y-2">
          <p class="text-xs text-muted-foreground">{{ t("settings.syncImportEndpointChangesHint") }}</p>
          <ul class="max-h-64 space-y-2 overflow-y-auto" data-sync-import-endpoint-changes>
            <li v-for="connection in endpointChanges" :key="connection.connectionId" class="rounded-md border bg-muted/30 px-3 py-2">
              <p class="text-xs font-medium">{{ connection.connectionName || connection.connectionId }}</p>
              <ul class="mt-1 space-y-1">
                <li v-for="change in connection.changes" :key="change.field" class="grid grid-cols-[minmax(0,7rem)_minmax(0,1fr)] items-start gap-2 text-[11px]">
                  <span class="truncate font-mono text-muted-foreground" :title="change.field">{{ change.field }}</span>
                  <span class="flex min-w-0 flex-wrap items-center gap-1 font-mono">
                    <span class="break-all line-through decoration-muted-foreground/60">{{ displayValue(change.before) }}</span>
                    <ArrowRight class="h-3 w-3 shrink-0 text-muted-foreground" aria-hidden="true" />
                    <span class="break-all font-medium">{{ displayValue(change.after) }}</span>
                  </span>
                </li>
              </ul>
            </li>
          </ul>
        </div>
      </div>

      <DialogFooter class="flex-col gap-2 sm:flex-col sm:space-x-0">
        <Button type="button" data-sync-import-keep @click="choose(true)">{{ t("settings.syncImportKeepCredentials") }}</Button>
        <Button v-if="endpointChanges.length" type="button" variant="destructive" data-sync-import-clear @click="choose(false)">{{ t("settings.syncImportClearCredentials") }}</Button>
        <Button type="button" variant="outline" data-sync-import-cancel @click="onOpenChange(false)">{{ t("dangerDialog.cancel") }}</Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>
