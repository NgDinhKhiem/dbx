import { RangeSet, RangeSetBuilder, StateField, type EditorState, type Extension } from "@codemirror/state";
import { Decoration, EditorView, GutterMarker, gutter, type DecorationSet } from "@codemirror/view";
import { createRunStatementButtonDom } from "@/lib/editor/editorThemes";
import { consoleRequestsForSelection, parseConsoleRequests, type ConsoleRequest } from "@/lib/elasticsearch/console/consoleRequests";
import type { DatabaseType } from "@/types/database";

export interface ConsoleActiveRequestState {
  requests: ConsoleRequest[];
  active: ConsoleRequest[];
}

export interface ConsoleActiveRequestOptions {
  databaseType?: DatabaseType;
  runLabel: string;
  onRun: (requests: ConsoleRequest[]) => void;
}

function activeFor(state: EditorState, requests: ConsoleRequest[]): ConsoleRequest[] {
  const selection = state.selection.main;
  return consoleRequestsForSelection(requests, selection.from, selection.to);
}

class RunMarker extends GutterMarker {
  constructor(private readonly label: string) {
    super();
  }

  override eq(other: GutterMarker): boolean {
    return other instanceof RunMarker && other.label === this.label;
  }

  override toDOM(): Node {
    const button = createRunStatementButtonDom(this.label);
    button.title = this.label;
    button.dataset.testid = "es-console-gutter-run";
    return button;
  }
}

const activeLine = Decoration.line({ class: "cm-es-console-active-request" });

function buildDecorations(state: EditorState, active: readonly ConsoleRequest[]): DecorationSet {
  const builder = new RangeSetBuilder<Decoration>();
  for (const request of active) {
    const first = state.doc.lineAt(request.from).number;
    const last = state.doc.lineAt(Math.max(request.from, request.to)).number;
    for (let line = first; line <= last; line += 1) {
      const { from } = state.doc.line(line);
      builder.add(from, from, activeLine);
    }
  }
  return builder.finish();
}

/**
 * Tracks the request(s) under the cursor/selection: highlights them and puts a
 * run button in the gutter on their first line.
 */
export function consoleActiveRequestExtension(options: ConsoleActiveRequestOptions): { extension: Extension; field: StateField<ConsoleActiveRequestState> } {
  const field = StateField.define<ConsoleActiveRequestState>({
    create(state) {
      const requests = parseConsoleRequests(state.doc.toString(), options.databaseType);
      return { requests, active: activeFor(state, requests) };
    },
    update(value, transaction) {
      if (!transaction.docChanged && !transaction.selection) return value;
      const requests = transaction.docChanged ? parseConsoleRequests(transaction.state.doc.toString(), options.databaseType) : value.requests;
      const active = activeFor(transaction.state, requests);
      const unchanged = !transaction.docChanged && active.length === value.active.length && active.every((request, index) => request === value.active[index]);
      return unchanged ? value : { requests, active };
    },
  });

  const runMarker = new RunMarker(options.runLabel);
  const decorations = EditorView.decorations.compute([field], (state) => buildDecorations(state, state.field(field).active));

  const runGutter = gutter({
    class: "cm-es-console-run-gutter",
    markers(view) {
      const { active } = view.state.field(field);
      if (!active.length) return RangeSet.empty;
      const lineStart = view.state.doc.lineAt(active[0].from).from;
      return RangeSet.of([runMarker.range(lineStart)]);
    },
    initialSpacer: () => runMarker,
    domEventHandlers: {
      mousedown(view, line, event) {
        const target = event.target as HTMLElement | null;
        if (!target?.closest?.(".cm-run-statement-marker")) return false;
        const { active } = view.state.field(field);
        if (!active.length || view.state.doc.lineAt(active[0].from).from !== line.from) return false;
        event.preventDefault();
        options.onRun(active);
        return true;
      },
    },
  });

  const theme = EditorView.theme({
    ".cm-es-console-active-request": {
      backgroundColor: "color-mix(in oklch, var(--primary) 7%, transparent)",
    },
    ".cm-es-console-run-gutter .cm-gutterElement": {
      display: "flex",
      alignItems: "center",
      justifyContent: "center",
      padding: "0 2px",
    },
  });

  return { extension: [field, decorations, runGutter, theme], field };
}
