<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { useConnectionStore } from "@/stores/connectionStore";
import { useToast } from "@/composables/useToast";
import { tableVGroupNodeId, tableVGroupPatternError, tableVGroupPatternMatches } from "@/lib/table/tableVGroup";
import {
  showTableVGroupDialog,
  tableVGroupName,
  tableVGroupDialogScope,
  tableVGroupDialogParentGroupId,
  tableVGroupDialogTableNames,
  tableVGroupDialogRowType,
  tableVGroupDialogRuleMode,
  tableVGroupDialogPattern,
  tableVGroupDialogIgnoreCase,
  tableVGroupDialogEditGroupId,
  tableVGroupDialogCandidateNames,
  tableVGroupDialogKind,
  showTableVGroupDeleteConfirm,
  tableVGroupDeleteTarget,
} from "./sidebarTreeDialogState";

const PREVIEW_LIMIT = 12;

const { t } = useI18n();
const { toast } = useToast();
const connectionStore = useConnectionStore();

const emit = defineEmits<{ created: [groupId: string] }>();

const deleteConfirmMessage = computed(() => t("tableVGroup.deleteGroupConfirmMessage", { name: tableVGroupDeleteTarget.value?.name ?? "" }));

const isEditingRule = computed(() => !!tableVGroupDialogEditGroupId.value);
const isDatabaseKind = computed(() => tableVGroupDialogKind.value === "databases");

const dialogTitle = computed(() => {
  if (isEditingRule.value) return t("tableVGroup.editRule");
  if (tableVGroupDialogRuleMode.value) return isDatabaseKind.value ? t("tableVGroup.newDatabaseRuleGroup") : t("tableVGroup.newRuleGroup");
  if (tableVGroupDialogTableNames.value.length) return t("tableVGroup.moveToNewGroup");
  if (tableVGroupDialogParentGroupId.value) return t("tableVGroup.newSubgroup");
  return isDatabaseKind.value ? t("tableVGroup.newDatabaseGroup") : t("tableVGroup.newSubgroup");
});

const patternError = computed(() => {
  if (!tableVGroupDialogRuleMode.value) return null;
  const pattern = tableVGroupDialogPattern.value;
  if (!pattern.trim()) return null;
  const error = tableVGroupPatternError(pattern, tableVGroupDialogIgnoreCase.value);
  if (!error) return null;
  return error === "too_long" ? t("tableVGroup.patternTooLong") : t("tableVGroup.patternInvalid", { message: error });
});

const previewMatches = computed(() => {
  if (!tableVGroupDialogRuleMode.value || patternError.value || !tableVGroupDialogPattern.value.trim()) return [];
  return tableVGroupPatternMatches(tableVGroupDialogCandidateNames.value, tableVGroupDialogPattern.value, tableVGroupDialogIgnoreCase.value);
});

const canConfirm = computed(() => {
  if (tableVGroupDialogRuleMode.value && (!tableVGroupDialogPattern.value.trim() || patternError.value)) return false;
  return isEditingRule.value || !!tableVGroupName.value.trim();
});

function currentRule() {
  return tableVGroupDialogRuleMode.value ? { pattern: tableVGroupDialogPattern.value, ignoreCase: tableVGroupDialogIgnoreCase.value } : null;
}

function close() {
  showTableVGroupDialog.value = false;
  tableVGroupName.value = "";
}

function confirmCreate() {
  const scope = tableVGroupDialogScope.value;
  if (!scope || !canConfirm.value) return;
  if (isEditingRule.value) {
    connectionStore.setTableVGroupRule(scope, tableVGroupDialogEditGroupId.value!, currentRule());
    close();
    return;
  }
  const name = tableVGroupName.value.trim();
  const groupId = connectionStore.createTableVGroup(scope, name, tableVGroupDialogParentGroupId.value, currentRule());
  if (groupId) {
    // Without an explicit row type the scope is the right-clicked source row,
    // whose type is that of the rows being moved (multi-select keeps one type).
    const rowType = tableVGroupDialogRowType.value ?? ("type" in scope ? scope.type : undefined);
    for (const tableName of tableVGroupDialogTableNames.value) {
      connectionStore.moveTableToVGroup(scope, tableName, groupId, rowType);
    }
    emit("created", tableVGroupNodeId(groupId));
  }
  close();
}

function clearRule() {
  const scope = tableVGroupDialogScope.value;
  if (!scope || !tableVGroupDialogEditGroupId.value) return;
  connectionStore.setTableVGroupRule(scope, tableVGroupDialogEditGroupId.value, null);
  close();
}

function confirmDelete() {
  const target = tableVGroupDeleteTarget.value;
  showTableVGroupDeleteConfirm.value = false;
  tableVGroupDeleteTarget.value = null;
  if (!target) return;
  connectionStore.deleteTableVGroups(target.scope, [target.groupId]);
  toast(t("tableVGroup.groupDeleted"), 2000);
}
</script>

<template>
  <Dialog v-model:open="showTableVGroupDialog">
    <DialogContent :class="tableVGroupDialogRuleMode ? 'max-w-md' : 'max-w-sm'">
      <DialogHeader>
        <DialogTitle>{{ dialogTitle }}</DialogTitle>
      </DialogHeader>
      <Input v-if="!isEditingRule" v-model="tableVGroupName" :placeholder="t('connectionGroup.groupNamePlaceholder')" data-testid="vgroup-name" @keydown.enter.prevent="confirmCreate" />
      <div v-if="tableVGroupDialogRuleMode" class="grid gap-2">
        <Label for="vgroup-pattern" class="text-xs">{{ t("tableVGroup.patternLabel") }}</Label>
        <Input id="vgroup-pattern" v-model="tableVGroupDialogPattern" class="font-mono text-xs" :placeholder="isDatabaseKind ? '^dev_|_dev$' : '^order_'" data-testid="vgroup-pattern" @keydown.enter.prevent="confirmCreate" />
        <div class="flex items-center justify-between gap-2">
          <Label for="vgroup-ignore-case" class="text-xs font-normal text-muted-foreground">{{ t("tableVGroup.patternIgnoreCase") }}</Label>
          <Switch id="vgroup-ignore-case" v-model="tableVGroupDialogIgnoreCase" />
        </div>
        <p v-if="patternError" class="m-0 text-xs text-destructive" data-testid="vgroup-pattern-error">{{ patternError }}</p>
        <template v-else-if="tableVGroupDialogPattern.trim()">
          <p class="m-0 text-xs text-muted-foreground" data-testid="vgroup-pattern-count">
            {{ t("tableVGroup.patternMatchCount", { count: previewMatches.length, total: tableVGroupDialogCandidateNames.length }) }}
          </p>
          <ul v-if="previewMatches.length" class="m-0 max-h-40 list-none overflow-y-auto rounded-md border border-border p-1.5 font-mono text-xs" data-testid="vgroup-pattern-preview">
            <li v-for="name in previewMatches.slice(0, PREVIEW_LIMIT)" :key="name" class="truncate">{{ name }}</li>
            <li v-if="previewMatches.length > PREVIEW_LIMIT" class="text-muted-foreground">{{ t("tableVGroup.patternMoreMatches", { count: previewMatches.length - PREVIEW_LIMIT }) }}</li>
          </ul>
        </template>
        <p class="m-0 text-xs leading-5 text-muted-foreground">{{ t("tableVGroup.patternHint") }}</p>
      </div>
      <DialogFooter>
        <Button v-if="isEditingRule" variant="ghost" class="mr-auto" @click="clearRule">{{ t("tableVGroup.clearRule") }}</Button>
        <Button variant="outline" @click="close">{{ t("dangerDialog.cancel") }}</Button>
        <Button :disabled="!canConfirm" data-testid="vgroup-confirm" @click="confirmCreate">{{ isEditingRule ? t("tableVGroup.saveRule") : t("connectionGroup.createGroup") }}</Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>

  <Dialog v-model:open="showTableVGroupDeleteConfirm">
    <DialogContent class="max-w-sm">
      <DialogHeader>
        <DialogTitle>{{ t("tableVGroup.deleteGroupConfirmTitle") }}</DialogTitle>
      </DialogHeader>
      <p class="text-sm text-muted-foreground">{{ deleteConfirmMessage }}</p>
      <DialogFooter>
        <Button variant="outline" @click="showTableVGroupDeleteConfirm = false">{{ t("dangerDialog.cancel") }}</Button>
        <Button variant="destructive" @click="confirmDelete">{{ t("tableVGroup.deleteGroup") }}</Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>
