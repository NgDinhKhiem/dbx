const SPLIT_RATIO_STORAGE_KEY = "dbx.esConsole.splitRatio";
export const DEFAULT_CONSOLE_SPLIT_RATIO = 0.5;
const MIN_RATIO = 0.2;
const MAX_RATIO = 0.8;

export function clampConsoleSplitRatio(ratio: number): number {
  if (!Number.isFinite(ratio)) return DEFAULT_CONSOLE_SPLIT_RATIO;
  return Math.min(MAX_RATIO, Math.max(MIN_RATIO, ratio));
}

export function loadConsoleSplitRatio(): number {
  try {
    const raw = globalThis.localStorage?.getItem(SPLIT_RATIO_STORAGE_KEY);
    return raw ? clampConsoleSplitRatio(Number(raw)) : DEFAULT_CONSOLE_SPLIT_RATIO;
  } catch {
    return DEFAULT_CONSOLE_SPLIT_RATIO;
  }
}

export function saveConsoleSplitRatio(ratio: number): void {
  try {
    globalThis.localStorage?.setItem(SPLIT_RATIO_STORAGE_KEY, String(clampConsoleSplitRatio(ratio)));
  } catch {
    // Storage unavailable: the ratio simply is not remembered.
  }
}
