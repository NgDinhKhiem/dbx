import { StreamLanguage, foldService, type StringStream } from "@codemirror/language";
import type { EditorState } from "@codemirror/state";

interface ConsoleModeState {
  inTripleString: boolean;
  inBlockComment: boolean;
  /** The previous token on this line was the request method. */
  afterMethod: boolean;
  /** The path token has been read; query string may follow. */
  inPath: boolean;
}

const METHOD = /^(?:GET|POST|PUT|DELETE|HEAD|PATCH)(?=\s)/i;

function onlyWhitespaceBefore(stream: StringStream): boolean {
  return stream.string.slice(0, stream.start).trim() === "";
}

/**
 * Tokenizer for the Kibana console syntax: `METHOD path` request lines, JSON /
 * NDJSON bodies (with `"""` strings), and `#`, `//`, block comments. Also used
 * for multi-response output, whose `# METHOD path  status` headers are comments.
 */
export const consoleStreamParser = {
  name: "es-console",
  startState: (): ConsoleModeState => ({ inTripleString: false, inBlockComment: false, afterMethod: false, inPath: false }),
  copyState: (state: ConsoleModeState): ConsoleModeState => ({ ...state }),
  token(stream: StringStream, state: ConsoleModeState): string | null {
    if (stream.sol()) {
      state.afterMethod = false;
      state.inPath = false;
    }
    if (state.inBlockComment) {
      if (stream.skipTo("*/")) {
        stream.match("*/");
        state.inBlockComment = false;
      } else stream.skipToEnd();
      return "comment";
    }
    if (state.inTripleString) {
      if (stream.skipTo('"""')) {
        stream.match('"""');
        state.inTripleString = false;
      } else stream.skipToEnd();
      return "string";
    }
    if (stream.eatSpace()) return null;

    if (state.inPath) {
      state.inPath = false;
      if (stream.match(/^\?\S*/)) return "attributeName";
    }
    if (state.afterMethod) {
      state.afterMethod = false;
      if (stream.match(/^[^\s?]+/)) {
        state.inPath = true;
        return "typeName";
      }
      if (stream.match(/^\?\S*/)) return "attributeName";
    }

    if (onlyWhitespaceBefore(stream)) {
      if (stream.match("#") || stream.match("//")) {
        stream.skipToEnd();
        return "comment";
      }
      if (stream.match(METHOD)) {
        state.afterMethod = true;
        return "keyword";
      }
    }
    if (stream.match("/*")) {
      state.inBlockComment = true;
      return "comment";
    }
    if (stream.match('"""')) {
      state.inTripleString = true;
      if (stream.skipTo('"""')) {
        stream.match('"""');
        state.inTripleString = false;
      } else stream.skipToEnd();
      return "string";
    }
    if (stream.match(/^"(?:[^"\\]|\\.)*"?/)) {
      return stream.match(/^\s*:/, false) ? "propertyName" : "string";
    }
    if (stream.match(/^-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/)) return "number";
    if (stream.match(/^(?:true|false)\b/)) return "bool";
    if (stream.match(/^null\b/)) return "null";
    const char = stream.next();
    if (char === "{" || char === "}") return "brace";
    if (char === "[" || char === "]") return "squareBracket";
    if (char === ":" || char === ",") return "punctuation";
    return null;
  },
  languageData: {
    commentTokens: { line: "#" },
    closeBrackets: { brackets: ["(", "[", "{", '"'] },
  },
};

export const consoleLanguage = StreamLanguage.define(consoleStreamParser);

const FOLD_SCAN_LIMIT = 4 * 1024 * 1024;

/**
 * Finds the bracket closing the one at `openPos`, skipping JSON strings.
 * Returns -1 when unbalanced or too far away.
 */
export function findMatchingJsonBracket(text: string, openPos: number, limit = FOLD_SCAN_LIMIT): number {
  const open = text[openPos];
  if (open !== "{" && open !== "[") return -1;
  let depth = 0;
  let inString = false;
  const end = Math.min(text.length, openPos + limit);
  for (let index = openPos; index < end; index += 1) {
    const char = text[index];
    if (inString) {
      if (char === "\\") index += 1;
      else if (char === '"') inString = false;
      continue;
    }
    if (char === '"') inString = true;
    else if (char === "{" || char === "[") depth += 1;
    else if (char === "}" || char === "]") {
      depth -= 1;
      if (depth === 0) return index;
    }
  }
  return -1;
}

/** Folds JSON objects/arrays whose opening bracket ends a line (the StreamLanguage has no syntax tree). */
export const consoleBraceFolding = foldService.of((state: EditorState, lineStart: number, lineEnd: number) => {
  const lineText = state.sliceDoc(lineStart, lineEnd).replace(/\s+$/, "");
  const last = lineText[lineText.length - 1];
  if (last !== "{" && last !== "[") return null;
  const openPos = lineStart + lineText.length - 1;
  const text = state.sliceDoc(openPos, Math.min(state.doc.length, openPos + FOLD_SCAN_LIMIT));
  const close = findMatchingJsonBracket(text, 0);
  if (close < 0) return null;
  const closePos = openPos + close;
  if (state.doc.lineAt(closePos).number === state.doc.lineAt(openPos).number) return null;
  return { from: openPos + 1, to: closePos };
});
