<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { Check, Copy, Loader2, Play, ShieldAlert, Square } from "@lucide/vue";
import DangerConfirmDialog from "@/components/editor/DangerConfirmDialog.vue";
import { Button } from "@/components/ui/button";
import { useToast } from "@/composables/useToast";
import { translateBackendError } from "@/i18n/backend-errors";
import { isMacOS } from "@/lib/backend/platform";
import { copyToClipboard } from "@/lib/common/clipboard";
import { ensureReadOnlyWriteAccess } from "@/lib/database/readOnlyWriteAccess";
import { sendConsoleRequest } from "@/lib/elasticsearch/console/consoleApi";
import { consoleBaseUrl, consoleRequestToCurl } from "@/lib/elasticsearch/console/consoleCurl";
import { clearConsoleHistory, createConsoleHistoryEntry, loadConsoleHistory, recordConsoleHistory, type ConsoleHistoryEntry } from "@/lib/elasticsearch/console/consoleHistory";
import { consoleRequestLabel, DEFAULT_CONSOLE_TEXT, type ConsoleRequest } from "@/lib/elasticsearch/console/consoleRequests";
import { consoleResponseDocument, consoleStatusText, consoleStatusTone, isReadOnlyConsoleError, type ConsoleResponseEntry, type ConsoleResponseLanguage, type ConsoleStatusTone } from "@/lib/elasticsearch/console/consoleResponse";
import { assessConsoleRequests } from "@/lib/elasticsearch/console/consoleSafety";
import { loadConsoleSplitRatio, saveConsoleSplitRatio, clampConsoleSplitRatio } from "@/lib/elasticsearch/console/consoleSplit";
import { useConnectionStore } from "@/stores/connectionStore";
import { useProductionSafetyStore } from "@/stores/productionSafetyStore";
import { useSettingsStore } from "@/stores/settingsStore";
import ConsoleHelpPopover from "./ConsoleHelpPopover.vue";
import ConsoleHistoryPopover from "./ConsoleHistoryPopover.vue";
import ConsoleRequestEditor from "./ConsoleRequestEditor.vue";
import ConsoleResponseViewer from "./ConsoleResponseViewer.vue";

const TEXT_CHANGE_DEBOUNCE_MS = 400;

const props = defineProps<{
  connectionId: string;
  initialText?: string;
}>();

const emit = defineEmits<{
  "text-change": [text: string];
}>();

const { t } = useI18n();
const { toast } = useToast();
const connectionStore = useConnectionStore();
const settingsStore = useSettingsStore();
const productionSafetyStore = useProductionSafetyStore();

const connection = computed(() => connectionStore.getConfig(props.connectionId));
const databaseType = computed(() => connection.value?.db_type);
const shortcutMod = isMacOS() ? "⌘" : "Ctrl";

const text = ref(props.initialText ?? DEFAULT_CONSOLE_TEXT);
const editorRef = ref<InstanceType<typeof ConsoleRequestEditor>>();
const splitRoot = ref<HTMLElement>();
const splitRatio = ref(loadConsoleSplitRatio());
const resizing = ref(false);

const running = ref(false);
const responses = ref<ConsoleResponseEntry[]>([]);
const responseCopied = ref(false);
const history = ref<ConsoleHistoryEntry[]>([]);

const dangerOpen = ref(false);
const dangerText = ref("");
let resolveDanger: ((confirmed: boolean) => void) | undefined;

let runSequence = 0;
let disposed = false;
let textChangeTimer: ReturnType<typeof setTimeout> | undefined;
let copiedTimer: ReturnType<typeof setTimeout> | undefined;

// ---- text persistence -------------------------------------------------------

function flushTextChange() {
  if (textChangeTimer === undefined) return;
  clearTimeout(textChangeTimer);
  textChangeTimer = undefined;
  emit("text-change", text.value);
}

function onTextUpdate(value: string) {
  text.value = value;
  if (textChangeTimer !== undefined) clearTimeout(textChangeTimer);
  textChangeTimer = setTimeout(() => {
    textChangeTimer = undefined;
    emit("text-change", text.value);
  }, TEXT_CHANGE_DEBOUNCE_MS);
}

// ---- history ----------------------------------------------------------------

watch(
  () => props.connectionId,
  (connectionId) => {
    history.value = loadConsoleHistory(connectionId);
  },
  { immediate: true },
);

function recordHistory(requests: readonly ConsoleRequest[]) {
  history.value = recordConsoleHistory(
    props.connectionId,
    requests.map((request) => createConsoleHistoryEntry(consoleRequestLabel(request), request.text)),
  );
}

function clearHistory() {
  clearConsoleHistory(props.connectionId);
  history.value = [];
}

function insertHistoryEntry(entry: ConsoleHistoryEntry) {
  editorRef.value?.insertRequest(entry.text);
}

// ---- safety -------------------------------------------------------------------

function requestDangerConfirmation(requestText: string): Promise<boolean> {
  resolveDanger?.(false);
  dangerText.value = requestText;
  dangerOpen.value = true;
  return new Promise<boolean>((resolve) => {
    resolveDanger = resolve;
  });
}

function settleDanger(confirmed: boolean) {
  const resolve = resolveDanger;
  resolveDanger = undefined;
  dangerOpen.value = false;
  resolve?.(confirmed);
}

function onDangerOpenChange(open: boolean) {
  if (!open) settleDanger(false);
}

/**
 * Same gate order as the query tab: read-only unlock, then the production
 * confirmation (always asked, not suppressible), then the dangerous-operation
 * confirmation (honours the "Confirm before dangerous operations" setting).
 */
async function confirmRequests(requests: readonly ConsoleRequest[]): Promise<boolean> {
  const currentConnection = connection.value;
  const allText = requests.map((request) => request.text).join("\n\n");
  if (!(await ensureReadOnlyWriteAccess({ connection: currentConnection, sql: allText, source: t("esConsole.source") }))) return false;

  const assessment = assessConsoleRequests(requests, currentConnection);
  if (!assessment.risky.length) return true;
  const riskyText = assessment.risky.map((request) => request.text).join("\n\n");
  if (assessment.production.active) {
    return productionSafetyStore.requestConfirmation({
      sql: riskyText,
      connectionName: currentConnection?.name,
      productionDatabases: assessment.production.databases,
      source: t("esConsole.source"),
    });
  }
  if (!settingsStore.editorSettings.confirmDangerousSqlExecution) return true;
  return requestDangerConfirmation(riskyText);
}

// ---- execution ------------------------------------------------------------------

function rawErrorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error && typeof error === "object") {
    const record = error as { message?: unknown; code?: unknown };
    return [record.code, record.message].filter((part) => typeof part === "string").join(" ") || JSON.stringify(error);
  }
  return String(error);
}

function errorEntry(request: ConsoleRequest, error: unknown): ConsoleResponseEntry {
  const raw = rawErrorMessage(error);
  const message = translateBackendError(t, error) || raw;
  const readOnly = isReadOnlyConsoleError(raw) || isReadOnlyConsoleError(message);
  return {
    label: consoleRequestLabel(request),
    body: "",
    error: readOnly ? `${t("esConsole.readOnlyError")}\n\n${message}` : message,
    readOnly,
  };
}

async function runRequests(requests: ConsoleRequest[]) {
  if (!requests.length) {
    toast(t("esConsole.noRequest"), 3000);
    return;
  }
  flushTextChange();
  if (!(await confirmRequests(requests)) || disposed) return;

  const sequence = ++runSequence;
  running.value = true;
  responses.value = [];
  recordHistory(requests);
  const entries: ConsoleResponseEntry[] = [];
  try {
    for (const request of requests) {
      let entry: ConsoleResponseEntry;
      try {
        const response = await sendConsoleRequest(props.connectionId, { method: request.method, path: request.path, body: request.body });
        entry = { label: consoleRequestLabel(request), status: response.status, tookMs: response.tookMs, body: response.body };
      } catch (error) {
        entry = errorEntry(request, error);
      }
      // A newer run (or cancel/unmount) owns the pane now: drop this stale result.
      if (sequence !== runSequence || disposed) return;
      entries.push(entry);
      responses.value = [...entries];
    }
  } finally {
    if (sequence === runSequence && !disposed) running.value = false;
  }
}

function runActive() {
  editorRef.value?.runActive();
}

function cancelRun() {
  runSequence += 1;
  running.value = false;
}

watch(
  () => props.connectionId,
  () => {
    // Responses belong to the previous connection; drop them and any run in flight.
    cancelRun();
    responses.value = [];
  },
);

watch(
  () => props.initialText,
  (value) => {
    if (value === undefined || value === text.value) return;
    text.value = value;
    editorRef.value?.setText(value);
  },
);

// ---- response ---------------------------------------------------------------------

const responseDocument = computed(() => consoleResponseDocument(responses.value));
const responseLanguage = computed<ConsoleResponseLanguage>(() => {
  if (responses.value.length > 1) return "console";
  return responseDocument.value.isJson ? "json" : "text";
});
const lastResponse = computed(() => responses.value[responses.value.length - 1]);
const totalTookMs = computed(() => responses.value.reduce((sum, entry) => sum + (entry.tookMs ?? 0), 0));
const readOnlyFailure = computed(() => responses.value.some((entry) => entry.readOnly));

const TONE_CLASSES: Record<ConsoleStatusTone, string> = {
  success: "bg-emerald-500/15 text-emerald-700 dark:text-emerald-400",
  redirect: "bg-sky-500/15 text-sky-700 dark:text-sky-400",
  "client-error": "bg-amber-500/15 text-amber-700 dark:text-amber-400",
  "server-error": "bg-destructive/15 text-destructive",
  error: "bg-destructive/15 text-destructive",
};

const statusBadge = computed(() => {
  const entry = lastResponse.value;
  if (!entry) return null;
  const tone = consoleStatusTone(entry.status);
  const label = entry.status === undefined ? t("esConsole.requestFailed") : `${entry.status}${consoleStatusText(entry.status) ? ` - ${consoleStatusText(entry.status)}` : ""}`;
  return { label, className: TONE_CLASSES[tone] };
});

async function copyResponse() {
  try {
    await copyToClipboard(responseDocument.value.text);
    responseCopied.value = true;
    if (copiedTimer !== undefined) clearTimeout(copiedTimer);
    copiedTimer = setTimeout(() => {
      responseCopied.value = false;
    }, 1500);
  } catch (error) {
    toast(t("esConsole.copyFailed", { message: rawErrorMessage(error) }), 4000);
  }
}

async function copyCurl(requests: ConsoleRequest[]) {
  const baseUrl = consoleBaseUrl(connection.value);
  try {
    await copyToClipboard(requests.map((request) => consoleRequestToCurl(request, baseUrl)).join("\n\n"));
    toast(t("esConsole.curlCopied"));
  } catch (error) {
    toast(t("esConsole.copyFailed", { message: rawErrorMessage(error) }), 4000);
  }
}

function onFormatFailed(reason: "invalid-json" | "unsupported") {
  toast(reason === "invalid-json" ? t("esConsole.formatInvalidJson") : t("esConsole.formatUnsupported"), 4000);
}

// ---- splitter ------------------------------------------------------------------------

let pointerMoveHandler: ((event: PointerEvent) => void) | undefined;
let pointerUpHandler: (() => void) | undefined;

function stopResize() {
  if (pointerMoveHandler) window.removeEventListener("pointermove", pointerMoveHandler);
  if (pointerUpHandler) window.removeEventListener("pointerup", pointerUpHandler);
  pointerMoveHandler = undefined;
  pointerUpHandler = undefined;
  if (resizing.value) saveConsoleSplitRatio(splitRatio.value);
  resizing.value = false;
}

function startResize(event: PointerEvent) {
  const root = splitRoot.value;
  if (!root || event.button !== 0) return;
  event.preventDefault();
  resizing.value = true;
  pointerMoveHandler = (move: PointerEvent) => {
    const rect = root.getBoundingClientRect();
    if (rect.width <= 0) return;
    splitRatio.value = clampConsoleSplitRatio((move.clientX - rect.left) / rect.width);
  };
  pointerUpHandler = stopResize;
  window.addEventListener("pointermove", pointerMoveHandler);
  window.addEventListener("pointerup", pointerUpHandler);
}

function onSplitterKeydown(event: KeyboardEvent) {
  const step = event.shiftKey ? 0.1 : 0.02;
  if (event.key === "ArrowLeft") splitRatio.value = clampConsoleSplitRatio(splitRatio.value - step);
  else if (event.key === "ArrowRight") splitRatio.value = clampConsoleSplitRatio(splitRatio.value + step);
  else return;
  event.preventDefault();
  saveConsoleSplitRatio(splitRatio.value);
}

// ---- lifecycle -------------------------------------------------------------------------

onMounted(() => {
  disposed = false;
});

onBeforeUnmount(() => {
  disposed = true;
  runSequence += 1;
  flushTextChange();
  stopResize();
  if (copiedTimer !== undefined) clearTimeout(copiedTimer);
  resolveDanger?.(false);
  resolveDanger = undefined;
});

defineExpose({ runActive, cancelRun });
</script>

<template>
  <div class="flex h-full min-h-0 w-full flex-col bg-background text-foreground" data-testid="es-console">
    <div class="flex h-9 shrink-0 items-center gap-1 border-b border-border px-2">
      <Button size="sm" class="h-7 gap-1 px-2.5 text-xs" :disabled="running" :title="`${t('esConsole.runRequest')} (${shortcutMod}+Enter)`" data-testid="es-console-run" @click="runActive">
        <Loader2 v-if="running" class="h-3.5 w-3.5 animate-spin" />
        <Play v-else class="h-3.5 w-3.5" />
        {{ t("esConsole.run") }}
      </Button>
      <Button v-if="running" variant="ghost" size="sm" class="h-7 gap-1 px-2 text-xs" data-testid="es-console-cancel" @click="cancelRun">
        <Square class="h-3 w-3" />
        {{ t("esConsole.cancel") }}
      </Button>
      <ConsoleHistoryPopover :entries="history" @insert="insertHistoryEntry" @clear="clearHistory" />
      <ConsoleHelpPopover :shortcut-mod="shortcutMod" />
    </div>

    <div ref="splitRoot" class="flex min-h-0 flex-1" :class="{ 'cursor-col-resize select-none': resizing }">
      <div class="min-h-0 min-w-0 shrink-0" :style="{ width: `${splitRatio * 100}%` }">
        <ConsoleRequestEditor ref="editorRef" :model-value="text" :connection-id="connectionId" :database-type="databaseType" :shortcut-mod="shortcutMod" @update:model-value="onTextUpdate" @run="runRequests" @copy-curl="copyCurl" @format-failed="onFormatFailed" />
      </div>

      <div
        role="separator"
        aria-orientation="vertical"
        tabindex="0"
        :aria-label="t('esConsole.splitterLabel')"
        :aria-valuenow="Math.round(splitRatio * 100)"
        aria-valuemin="20"
        aria-valuemax="80"
        class="group relative w-1.5 shrink-0 cursor-col-resize border-x border-border bg-muted/40 hover:bg-primary/20 focus-visible:bg-primary/30 focus-visible:outline-none"
        :class="{ 'bg-primary/30': resizing }"
        data-testid="es-console-splitter"
        @pointerdown="startResize"
        @keydown="onSplitterKeydown"
      />

      <div class="flex min-h-0 min-w-0 flex-1 flex-col">
        <div class="flex h-8 shrink-0 items-center gap-2 border-b border-border px-2 text-xs">
          <span class="font-medium text-muted-foreground">{{ t("esConsole.responseLabel") }}</span>
          <span v-if="responses.length > 1" class="text-muted-foreground">{{ t("esConsole.responsesCount", { count: responses.length }) }}</span>
          <div class="ml-auto flex items-center gap-2">
            <Loader2 v-if="running" class="h-3.5 w-3.5 animate-spin text-muted-foreground" />
            <span v-if="statusBadge" class="rounded px-1.5 py-0.5 font-mono text-[11px] font-medium" :class="statusBadge.className" data-testid="es-console-status">{{ statusBadge.label }}</span>
            <span v-if="lastResponse && lastResponse.status !== undefined" class="font-mono text-[11px] text-muted-foreground" data-testid="es-console-took">{{ t("esConsole.took", { ms: totalTookMs }) }}</span>
            <Button variant="ghost" size="icon-xs" class="text-muted-foreground" :disabled="!responses.length" :title="t('esConsole.copyResponse')" :aria-label="t('esConsole.copyResponse')" data-testid="es-console-copy-response" @click="copyResponse">
              <Check v-if="responseCopied" class="h-3.5 w-3.5 text-emerald-600" />
              <Copy v-else class="h-3.5 w-3.5" />
            </Button>
          </div>
        </div>

        <div v-if="readOnlyFailure" class="flex shrink-0 items-start gap-2 border-b border-destructive/30 bg-destructive/10 px-3 py-2 text-xs text-destructive" role="alert" data-testid="es-console-read-only">
          <ShieldAlert class="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <div>
            <p class="font-medium">{{ t("esConsole.readOnlyTitle") }}</p>
            <p class="text-destructive/90">{{ t("esConsole.readOnlyError") }}</p>
          </div>
        </div>

        <div class="relative min-h-0 flex-1">
          <ConsoleResponseViewer :text="responseDocument.text" :language="responseLanguage" />
          <div v-if="!responses.length" class="pointer-events-none absolute inset-0 flex items-center justify-center p-6 text-center text-xs text-muted-foreground" data-testid="es-console-response-placeholder">
            {{ running ? t("esConsole.running") : t("esConsole.noResponse") }}
          </div>
        </div>
      </div>
    </div>

    <DangerConfirmDialog v-if="dangerOpen" :open="dangerOpen" :sql="dangerText" :message="t('esConsole.confirmMessage')" :confirm-label="t('esConsole.confirmAction')" :close-on-confirm="false" @update:open="onDangerOpenChange" @confirm="settleDanger(true)" />
  </div>
</template>
