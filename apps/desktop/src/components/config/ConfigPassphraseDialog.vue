<script setup lang="ts">
import { computed, nextTick, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { Lock } from "@lucide/vue";
import { Button } from "@/components/ui/button";
import PasswordInput from "@/components/ui/PasswordInput.vue";
import { Label } from "@/components/ui/label";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { getRememberedExportPassphrase } from "@/lib/backend/exportPassphraseSession";
import { MIN_EXPORT_PASSPHRASE_LENGTH } from "@/lib/backend/configCrypto";

const props = defineProps<{
  open: boolean;
  mode: "export" | "import";
  externalError?: string;
  busy?: boolean;
}>();

const emit = defineEmits<{
  "update:open": [value: boolean];
  confirm: [passphrase: string];
  requestUnencrypted: [];
}>();

const { t } = useI18n();
const dialogOpen = computed({
  get: () => props.open,
  set: (v) => emit("update:open", v),
});

const passphrase = ref("");
const passphraseConfirm = ref("");
const error = ref("");
// 密码短语输入框组件引用，打开对话框时用于手动聚焦
const passphraseInput = ref<InstanceType<typeof PasswordInput> | null>(null);

watch(
  dialogOpen,
  async (open) => {
    if (!open) return;
    // 导出模式回显本次会话上次使用的密码短语（仅内存保存，不落盘）；导入模式始终从空开始
    const remembered = props.mode === "export" ? getRememberedExportPassphrase() : "";
    passphrase.value = remembered;
    passphraseConfirm.value = remembered;
    error.value = "";
    await nextTick();
    // 手动聚焦并把光标定位到末尾，避免新输入插到回显内容前面
    const inputEl = passphraseInput.value?.$el?.querySelector("input");
    inputEl?.focus();
    const length = String(inputEl?.value ?? "").length;
    inputEl?.setSelectionRange(length, length);
  },
  { immediate: true },
);

function confirm() {
  if (props.busy) return;
  if (!passphrase.value) {
    error.value = t("configExport.passphraseRequired");
    return;
  }
  if (props.mode === "export" && passphrase.value !== passphraseConfirm.value) {
    error.value = t("configExport.passphraseMismatch");
    return;
  }
  // Only new exports enforce the minimum; existing files decrypt with whatever
  // passphrase they were created with.
  if (props.mode === "export" && passphrase.value.length < MIN_EXPORT_PASSPHRASE_LENGTH) {
    error.value = t("configExport.passphraseTooShortMin", { min: MIN_EXPORT_PASSPHRASE_LENGTH });
    return;
  }
  emit("confirm", passphrase.value);
}

const displayError = computed(() => error.value || props.externalError || "");
</script>

<template>
  <Dialog v-model:open="dialogOpen">
    <!-- initial-focus=false：禁用默认自动聚焦，改由打开对话框时的手动聚焦把光标定位到回显内容末尾 -->
    <DialogContent class="sm:max-w-[440px]" :initial-focus="false">
      <DialogHeader>
        <DialogTitle class="flex items-center gap-2">
          <Lock class="h-5 w-5" />
          {{ mode === "export" ? t("configExport.passphraseTitle") : t("configExport.passphraseImportTitle") }}
        </DialogTitle>
      </DialogHeader>

      <div class="grid gap-4 py-4">
        <p class="text-sm text-muted-foreground">
          {{ mode === "export" ? t("configExport.passphraseExportHint") : t("configExport.passphraseImportHint") }}
        </p>

        <div class="grid gap-2">
          <Label>{{ t("configExport.passphrase") }}</Label>
          <PasswordInput ref="passphraseInput" v-model="passphrase" :placeholder="t('configExport.passphrasePlaceholder')" :toggle-tab-index="-1" :disabled="busy" @keydown.enter="mode === 'import' ? confirm() : undefined" />
          <p v-if="mode === 'export'" class="text-xs text-muted-foreground">{{ t("configExport.passphraseMinLengthHint", { min: MIN_EXPORT_PASSPHRASE_LENGTH }) }}</p>
        </div>

        <div v-if="mode === 'export'" class="grid gap-2">
          <Label>{{ t("configExport.passphraseConfirm") }}</Label>
          <PasswordInput v-model="passphraseConfirm" :placeholder="t('configExport.passphraseConfirmPlaceholder')" :toggle-tab-index="-1" :disabled="busy" @keydown.enter="confirm" />
        </div>

        <p v-if="displayError" class="text-sm text-destructive">{{ displayError }}</p>
      </div>

      <DialogFooter>
        <Button v-if="mode === 'export'" type="button" variant="outline" :disabled="busy" @click="emit('requestUnencrypted')">
          {{ t("configExport.exportUnencrypted") }}
        </Button>
        <Button type="button" :disabled="busy" @click="confirm">
          {{ mode === "export" ? t("configExport.exportEncrypted") : t("configExport.decryptImport") }}
        </Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>
