<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, shallowRef } from "vue";
import { useI18n } from "vue-i18n";
import { Play, Settings } from "@lucide/vue";
import { EditorSelection, EditorState, Prec, type StateField } from "@codemirror/state";
import { EditorView, drawSelection, dropCursor, highlightActiveLineGutter, highlightSpecialChars, keymap, lineNumbers } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { bracketMatching, foldGutter, foldKeymap, indentOnInput } from "@codemirror/language";
import { autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap, snippet, type Completion, type CompletionContext, type CompletionResult } from "@codemirror/autocomplete";
import { Button } from "@/components/ui/button";
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuShortcut, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import { trimmedSelectionLayer } from "@/lib/editor/codemirrorTrimmedSelectionLayer";
import { sqlCompletionTheme } from "@/lib/editor/editorThemes";
import {
  buildElasticsearchCompletionItemsFromContext,
  elasticsearchCompletionNeedsFields,
  getElasticsearchCompletionContext,
  getElasticsearchCompletionResultValidFor,
  shouldAutoOpenElasticsearchCompletion,
  type ElasticsearchCompletionField,
  type ElasticsearchCompletionItem,
} from "@/lib/elasticsearch/elasticsearchCompletion";
import { consoleActiveRequestExtension, type ConsoleActiveRequestState } from "@/lib/elasticsearch/console/consoleActiveRequest";
import { consoleBraceFolding, consoleLanguage } from "@/lib/elasticsearch/console/consoleLanguage";
import { autoIndentConsoleRequest, type ConsoleRequest } from "@/lib/elasticsearch/console/consoleRequests";
import { useConnectionStore } from "@/stores/connectionStore";
import type { DatabaseType } from "@/types/database";
import { useConsoleEditorTheme } from "./useConsoleEditorTheme";

const props = defineProps<{
  modelValue: string;
  connectionId: string;
  databaseType?: DatabaseType;
  shortcutMod: string;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: string];
  run: [requests: ConsoleRequest[]];
  "copy-curl": [requests: ConsoleRequest[]];
  "format-failed": [reason: "invalid-json" | "unsupported"];
}>();

const { t } = useI18n();
const connectionStore = useConnectionStore();
const host = ref<HTMLElement>();
const view = shallowRef<EditorView | null>(null);
const overlayTop = ref<number | null>(null);
const actionMenuOpen = ref(false);
const { initialExtensions } = useConsoleEditorTheme(() => view.value);
let activeField: StateField<ConsoleActiveRequestState> | null = null;
let destroyed = false;
let scrollListenerTarget: HTMLElement | null = null;

function activeRequests(): ConsoleRequest[] {
  const current = view.value;
  if (!current || !activeField) return [];
  return current.state.field(activeField).active;
}

function allRequests(): ConsoleRequest[] {
  const current = view.value;
  if (!current || !activeField) return [];
  return current.state.field(activeField).requests;
}

function runActive(): boolean {
  const requests = activeRequests();
  if (requests.length) emit("run", requests);
  return true;
}

function copyCurl() {
  const requests = activeRequests();
  if (requests.length) emit("copy-curl", requests);
}

/** Auto-indents every active request; later requests first so earlier offsets stay valid. */
function autoIndent(): boolean {
  const current = view.value;
  const requests = activeRequests();
  if (!current || !requests.length) return true;
  const changes: { from: number; to: number; insert: string }[] = [];
  for (const request of requests) {
    const result = autoIndentConsoleRequest(request.text);
    if (!result.ok) {
      emit("format-failed", result.reason);
      return true;
    }
    if (result.text !== request.text) changes.push({ from: request.from, to: request.to, insert: result.text });
  }
  if (changes.length) {
    const cursor = requests[0].from;
    current.dispatch({ changes, selection: EditorSelection.cursor(cursor), scrollIntoView: true, userEvent: "input.format" });
  }
  return true;
}

function isModKey(event: KeyboardEvent): boolean {
  return (event.metaKey || event.ctrlKey) && !event.altKey && !event.shiftKey;
}

function clampBoost(boost: number): number {
  return Math.max(-99, Math.min(99, boost));
}

function replaceEnd(editor: EditorView, to: number, item: ElasticsearchCompletionItem): number {
  return item.replaceClosingQuote && editor.state.sliceDoc(to, to + 1) === item.replaceClosingQuote ? to + 1 : to;
}

function completionForItem(item: ElasticsearchCompletionItem): Completion {
  const base: Completion = {
    label: item.filterText ?? item.label,
    ...(item.filterText ? { displayLabel: item.label } : {}),
    detail: item.detail,
    info: item.info,
    type: item.type === "column" ? "property" : item.type,
    boost: clampBoost(item.boost),
  };
  if ((item.applyAsSnippet || item.type === "snippet") && item.apply) {
    const applySnippet = snippet(item.apply);
    return { ...base, apply: (editor, completion, from, to) => applySnippet(editor, completion, from, replaceEnd(editor, to, item)) };
  }
  return {
    ...base,
    apply(editor, _completion, from, to) {
      const insert = item.apply ?? item.label;
      editor.dispatch({ changes: { from, to: replaceEnd(editor, to, item), insert }, selection: { anchor: from + insert.length }, userEvent: "input.complete" });
    },
  };
}

async function completionSource(context: CompletionContext): Promise<CompletionResult | null> {
  const doc = context.state.doc.toString();
  const position = context.pos;
  if (!context.explicit && !shouldAutoOpenElasticsearchCompletion(doc, position)) return null;
  const completionContext = getElasticsearchCompletionContext(doc, position);
  let indices: string[] = [];
  let fields: ElasticsearchCompletionField[] = [];
  if (props.connectionId && completionContext.mode === "path") {
    try {
      indices = await connectionStore.listElasticsearchCompletionIndices(props.connectionId, "");
    } catch {
      indices = [];
    }
  }
  if (props.connectionId && elasticsearchCompletionNeedsFields(completionContext) && completionContext.index) {
    try {
      fields = await connectionStore.listElasticsearchCompletionFields(props.connectionId, completionContext.index);
    } catch {
      fields = [];
    }
  }
  if (context.aborted || destroyed) return null;
  const items = buildElasticsearchCompletionItemsFromContext(completionContext, { indices, fields });
  if (!items.length) return null;
  return { from: completionContext.from, options: items.map(completionForItem), validFor: getElasticsearchCompletionResultValidFor() };
}

function measureOverlay() {
  const current = view.value;
  if (!current || destroyed) return;
  current.requestMeasure({
    key: "es-console-action-overlay",
    read(editor) {
      const first = activeField ? editor.state.field(activeField).active[0] : undefined;
      const hostElement = host.value;
      if (!first || !hostElement) return null;
      try {
        const coords = editor.coordsAtPos(first.from, 1);
        if (!coords) return null;
        const scroller = editor.scrollDOM.getBoundingClientRect();
        if (coords.top < scroller.top - 1 || coords.bottom > scroller.bottom) return null;
        return coords.top - hostElement.getBoundingClientRect().top;
      } catch {
        return null;
      }
    },
    write(top) {
      if (!destroyed) overlayTop.value = top;
    },
  });
}

function onScroll() {
  measureOverlay();
}

async function create() {
  const themeExtensions = await initialExtensions();
  if (destroyed || !host.value) return;
  const active = consoleActiveRequestExtension({
    databaseType: props.databaseType,
    runLabel: t("esConsole.runRequest"),
    onRun: (requests) => emit("run", requests),
  });
  activeField = active.field;

  const state = EditorState.create({
    doc: props.modelValue,
    extensions: [
      Prec.highest(
        EditorView.domEventHandlers({
          keydown(event) {
            if (!isModKey(event)) return false;
            const key = event.key.toLowerCase();
            if (key !== "enter" && key !== "i") return false;
            event.preventDefault();
            event.stopPropagation();
            if (key === "enter") runActive();
            else autoIndent();
            return true;
          },
        }),
      ),
      lineNumbers(),
      active.extension,
      foldGutter(),
      highlightActiveLineGutter(),
      highlightSpecialChars(),
      history(),
      drawSelection(),
      trimmedSelectionLayer(),
      dropCursor(),
      EditorState.allowMultipleSelections.of(true),
      indentOnInput(),
      bracketMatching(),
      closeBrackets(),
      autocompletion({ override: [completionSource], activateOnTyping: true }),
      consoleLanguage,
      consoleBraceFolding,
      keymap.of([...closeBracketsKeymap, ...completionKeymap, ...defaultKeymap, ...historyKeymap, ...foldKeymap, indentWithTab]),
      sqlCompletionTheme(EditorView),
      ...themeExtensions,
      EditorView.theme({ "&": { height: "100%" }, "&.cm-focused": { outline: "none" } }),
      EditorView.contentAttributes.of({ "aria-label": t("esConsole.requestEditorLabel"), "data-testid": "es-console-request-editor" }),
      EditorView.updateListener.of((update) => {
        if (update.docChanged) emit("update:modelValue", update.state.doc.toString());
        if (update.docChanged || update.selectionSet || update.geometryChanged || update.viewportChanged) measureOverlay();
      }),
    ],
  });
  view.value = new EditorView({ state, parent: host.value });
  scrollListenerTarget = view.value.scrollDOM;
  scrollListenerTarget.addEventListener("scroll", onScroll, { passive: true });
  measureOverlay();
}

function destroy() {
  destroyed = true;
  scrollListenerTarget?.removeEventListener("scroll", onScroll);
  scrollListenerTarget = null;
  view.value?.destroy();
  view.value = null;
}

/** Inserts text (e.g. a history entry) as a new request after the current one, then selects it. */
function insertRequest(text: string) {
  const current = view.value;
  if (!current) return;
  const doc = current.state.doc;
  const active = activeRequests();
  const anchorRequest = active[active.length - 1];
  const insertAt = anchorRequest ? anchorRequest.to : doc.length;
  const before = doc.sliceString(0, insertAt);
  const prefix = before.length === 0 ? "" : before.endsWith("\n\n") ? "" : before.endsWith("\n") ? "\n" : "\n\n";
  const insert = `${prefix}${text.trim()}\n`;
  const requestStart = insertAt + prefix.length;
  current.dispatch({ changes: { from: insertAt, insert }, selection: EditorSelection.cursor(requestStart), scrollIntoView: true, userEvent: "input" });
  current.focus();
}

function setText(text: string) {
  const current = view.value;
  if (!current || current.state.doc.toString() === text) return;
  current.dispatch({ changes: { from: 0, to: current.state.doc.length, insert: text } });
}

function focus() {
  view.value?.focus();
}

onMounted(() => {
  void create();
});

onBeforeUnmount(destroy);

defineExpose({ activeRequests, allRequests, runActive, autoIndent, insertRequest, setText, focus, view });
</script>

<template>
  <div class="relative h-full min-h-0 w-full overflow-hidden">
    <div ref="host" class="h-full min-h-0 w-full" />
    <div v-if="overlayTop !== null" class="pointer-events-none absolute right-3 z-10 flex items-center gap-0.5" :style="{ top: `${overlayTop}px` }" data-testid="es-console-request-actions">
      <Button variant="ghost" size="icon-xs" class="pointer-events-auto h-5 w-5 text-emerald-600 dark:text-emerald-400" :title="`${t('esConsole.runRequest')} (${shortcutMod}+Enter)`" :aria-label="t('esConsole.runRequest')" @mousedown.prevent @click="runActive">
        <Play class="h-3.5 w-3.5" />
      </Button>
      <DropdownMenu v-model:open="actionMenuOpen">
        <DropdownMenuTrigger as-child>
          <Button variant="ghost" size="icon-xs" class="pointer-events-auto h-5 w-5 text-muted-foreground" :title="t('esConsole.requestActions')" :aria-label="t('esConsole.requestActions')" @mousedown.prevent>
            <Settings class="h-3.5 w-3.5" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" class="min-w-44">
          <DropdownMenuItem @select="copyCurl">{{ t("esConsole.copyAsCurl") }}</DropdownMenuItem>
          <DropdownMenuItem @select="autoIndent">
            {{ t("esConsole.autoIndent") }}
            <DropdownMenuShortcut>{{ shortcutMod }}+I</DropdownMenuShortcut>
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  </div>
</template>
