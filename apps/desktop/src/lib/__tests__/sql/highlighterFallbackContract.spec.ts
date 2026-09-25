import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const srcRoot = path.resolve(__dirname, "../../..");

function sourceFiles(): string[] {
  const found: string[] = [];
  const walk = (dir: string) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (entry.name === "node_modules" || entry.name === "__tests__") continue;
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (/\.(vue|ts)$/.test(entry.name) && !/\.(spec|test)\.ts$/.test(entry.name)) found.push(full);
    }
  };
  walk(srcRoot);
  return found;
}

// Matches `someHighlighter.value?.(x) ?? fallback` and `props.highlightJson?.(x) ?? fallback`.
const HIGHLIGHTER_FALLBACK = /[hH]ighlight\w*(?:\.value)?\?\.\([^\n]*?\)\s*\?\?\s*([^\n;]+)/g;

describe("v-html highlighter fallbacks", () => {
  it("escapes the raw text whenever a lazily-loaded highlighter is not ready yet", () => {
    // Highlighter output is bound with v-html. Returning the raw source while
    // shiki is still loading injects column names, comments, cell values, etc.
    // straight into the DOM.
    const offenders: string[] = [];
    let checked = 0;
    for (const file of sourceFiles()) {
      const source = readFileSync(file, "utf8");
      for (const match of source.matchAll(HIGHLIGHTER_FALLBACK)) {
        checked += 1;
        if (!/escape/i.test(match[1])) offenders.push(`${path.relative(srcRoot, file)}: ${match[0].trim()}`);
      }
    }
    expect(checked).toBeGreaterThan(0);
    expect(offenders).toEqual([]);
  });
});
