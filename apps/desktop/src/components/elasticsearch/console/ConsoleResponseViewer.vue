<script setup lang="ts">
import { onBeforeUnmount, onMounted, shallowRef, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { Compartment, EditorState, type Extension } from "@codemirror/state";
import { EditorView, drawSelection, highlightSpecialChars, keymap, lineNumbers } from "@codemirror/view";
import { defaultKeymap } from "@codemirror/commands";
import { foldGutter, foldKeymap } from "@codemirror/language";
import { json } from "@codemirror/lang-json";
import { trimmedSelectionLayer } from "@/lib/editor/codemirrorTrimmedSelectionLayer";
import { consoleBraceFolding, consoleLanguage } from "@/lib/elasticsearch/console/consoleLanguage";
import type { ConsoleResponseLanguage } from "@/lib/elasticsearch/console/consoleResponse";
import { useSettingsStore } from "@/stores/settingsStore";
import { useConsoleEditorTheme } from "./useConsoleEditorTheme";

const props = defineProps<{
  text: string;
  language: ConsoleResponseLanguage;
}>();

const { t } = useI18n();
const settingsStore = useSettingsStore();
const host = ref<HTMLElement>();
const view = shallowRef<EditorView | null>(null);
const languageCompartment = new Compartment();
const wrapCompartment = new Compartment();
const { initialExtensions } = useConsoleEditorTheme(() => view.value);
let destroyed = false;

function languageExtension(language: ConsoleResponseLanguage): Extension {
  if (language === "json") return json();
  if (language === "console") return [consoleLanguage, consoleBraceFolding];
  return [];
}

function wrapExtension(): Extension {
  return settingsStore.editorSettings.wordWrap ? EditorView.lineWrapping : [];
}

async function create() {
  const themeExtensions = await initialExtensions();
  if (destroyed || !host.value) return;
  view.value = new EditorView({
    parent: host.value,
    state: EditorState.create({
      doc: props.text,
      extensions: [
        lineNumbers(),
        foldGutter(),
        highlightSpecialChars(),
        drawSelection(),
        trimmedSelectionLayer(),
        EditorState.readOnly.of(true),
        EditorView.editable.of(false),
        EditorView.contentAttributes.of({ tabindex: "0", "aria-label": t("esConsole.responseLabel"), "data-testid": "es-console-response-editor" }),
        keymap.of([...defaultKeymap, ...foldKeymap]),
        languageCompartment.of(languageExtension(props.language)),
        wrapCompartment.of(wrapExtension()),
        ...themeExtensions,
        EditorView.theme({ "&": { height: "100%" }, "&.cm-focused": { outline: "none" } }),
      ],
    }),
  });
}

watch(
  () => [props.text, props.language] as const,
  ([text, language], [, previousLanguage]) => {
    const current = view.value;
    if (!current) return;
    const effects = language !== previousLanguage ? [languageCompartment.reconfigure(languageExtension(language))] : [];
    const changes = current.state.doc.toString() === text ? undefined : { from: 0, to: current.state.doc.length, insert: text };
    if (!changes && !effects.length) return;
    current.dispatch({ changes, effects, selection: changes ? { anchor: 0 } : undefined, scrollIntoView: Boolean(changes) });
  },
);

watch(
  () => settingsStore.editorSettings.wordWrap,
  () => view.value?.dispatch({ effects: wrapCompartment.reconfigure(wrapExtension()) }),
);

onMounted(() => {
  void create();
});

onBeforeUnmount(() => {
  destroyed = true;
  view.value?.destroy();
  view.value = null;
});

defineExpose({ view });
</script>

<template>
  <div ref="host" class="h-full min-h-0 w-full overflow-hidden" />
</template>
