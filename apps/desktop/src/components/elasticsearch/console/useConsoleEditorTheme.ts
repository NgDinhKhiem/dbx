import { watch } from "vue";
import { Compartment, type Extension } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { editorFontTheme, loadEditorTheme } from "@/lib/editor/editorThemes";
import { useTheme } from "@/composables/useTheme";
import { useSettingsStore } from "@/stores/settingsStore";

/**
 * Keeps a console CodeMirror view on the app's editor theme and font, the same
 * way the cell-detail editors do (`loadEditorTheme` + `editorFontTheme`).
 */
export function useConsoleEditorTheme(getView: () => EditorView | null) {
  const settingsStore = useSettingsStore();
  const { isDark, themePalette } = useTheme();
  const themeCompartment = new Compartment();
  const fontCompartment = new Compartment();

  function fontExtension(): Extension {
    const settings = settingsStore.editorSettings;
    return editorFontTheme(EditorView, settings.fontSize, settings.fontFamily, { fixedHeight: true, scrollable: true });
  }

  function loadTheme(): Promise<Extension> {
    return loadEditorTheme(settingsStore.editorSettings.theme, isDark.value ? "dark" : "light", undefined, themePalette.value);
  }

  async function initialExtensions(): Promise<Extension[]> {
    const theme = await loadTheme();
    return [themeCompartment.of(theme), fontCompartment.of(fontExtension())];
  }

  watch([() => settingsStore.editorSettings.theme, isDark, themePalette, () => settingsStore.editorSettings.fontSize, () => settingsStore.editorSettings.fontFamily], async () => {
    if (!getView()) return;
    const theme = await loadTheme();
    const view = getView();
    if (!view) return;
    view.dispatch({ effects: [themeCompartment.reconfigure(theme), fontCompartment.reconfigure(fontExtension())] });
  });

  return { initialExtensions };
}
