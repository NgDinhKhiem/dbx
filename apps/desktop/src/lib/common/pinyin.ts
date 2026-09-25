import { shallowRef } from "vue";

// pinyin-pro ships a large dictionary. It is only needed once text containing
// Han characters is matched, so it is loaded on demand (and preloaded when the
// app goes idle) instead of landing in the startup bundle.
type PinyinFn = (typeof import("pinyin-pro"))["pinyin"];

let pinyinImpl: PinyinFn | undefined;
let pinyinLoad: Promise<void> | undefined;
/**
 * Bumped once pinyin-pro has loaded. Reactive consumers (computed search
 * results) that matched Han text before then read it and re-evaluate.
 */
const pinyinReadyVersion = shallowRef(0);

/** Loads pinyin-pro; resolves immediately once loaded. Safe to call repeatedly. */
export function loadPinyin(): Promise<void> {
  if (pinyinImpl) return Promise.resolve();
  pinyinLoad ??= import("pinyin-pro").then(
    (module) => {
      pinyinImpl = module.pinyin;
      pinyinReadyVersion.value += 1;
    },
    (error) => {
      // Allow a later retry (e.g. a transient chunk load failure).
      pinyinLoad = undefined;
      throw error;
    },
  );
  return pinyinLoad;
}

export function isPinyinLoaded(): boolean {
  return pinyinImpl !== undefined;
}

/** Preloads pinyin-pro once the app is idle so the first Han search is complete. */
export function preloadPinyinWhenIdle(): void {
  if (pinyinImpl || pinyinLoad || typeof window === "undefined") return;
  const start = () => void loadPinyin().catch((error) => console.warn("[DBX][pinyin] Failed to load pinyin-pro:", error));
  if (typeof window.requestIdleCallback === "function") window.requestIdleCallback(start, { timeout: 10_000 });
  else window.setTimeout(start, 2_000);
}

const HAN_CHAR = /\p{Script=Han}/u;
const ASCII_ALNUM = /[a-z0-9]/i;

const firstLetterCache = new Map<string, string>();
const MAX_CACHE_ENTRIES = 100_000;

function cacheFirstLetters(text: string, result: string): void {
  if (firstLetterCache.size >= MAX_CACHE_ENTRIES) {
    const oldest = firstLetterCache.keys().next().value;
    if (oldest !== undefined) firstLetterCache.delete(oldest);
  }
  firstLetterCache.set(text, result);
}

export function containsHan(text: string): boolean {
  return HAN_CHAR.test(text);
}

/**
 * First pinyin letter of every Han character, with ASCII letters/digits kept
 * as-is (lowercased) and every other character dropped. Used for DataGrip-style
 * initials matching, e.g. pinyinFirstLetters("总租金") === "zzj".
 *
 * Only the default reading is used for polyphonic characters. Until pinyin-pro
 * has loaded (see `loadPinyin`), Han characters are skipped and the result is
 * not cached; reactive callers re-run automatically once it is available.
 */
export function pinyinFirstLetters(text: string): string {
  const cached = firstLetterCache.get(text);
  if (cached !== undefined) {
    firstLetterCache.delete(text);
    firstLetterCache.set(text, cached);
    return cached;
  }
  const pinyin = pinyinImpl;
  let result = "";
  let complete = true;
  for (const char of text) {
    if (HAN_CHAR.test(char)) {
      if (pinyin) {
        result += pinyin(char, { pattern: "first", toneType: "none" });
      } else {
        // Not loaded yet: Han characters contribute no initials for now.
        complete = false;
      }
    } else if (ASCII_ALNUM.test(char)) {
      result += char.toLowerCase();
    }
  }
  if (complete) {
    cacheFirstLetters(text, result);
  } else {
    // Track the load so reactive callers recompute with real initials, and
    // start it now in case the idle preload has not run yet.
    void pinyinReadyVersion.value;
    void loadPinyin().catch((error) => console.warn("[DBX][pinyin] Failed to load pinyin-pro:", error));
  }
  return result;
}

/** True when an ASCII-letters/digits query can match `candidate` via pinyin initials. */
export function matchesPinyinInitials(candidate: string, query: string): boolean {
  if (!/^[a-z0-9]+$/.test(query) || !containsHan(candidate)) return false;
  return pinyinFirstLetters(candidate).startsWith(query);
}

/**
 * Ordered-subsequence match of `query` against `text` (e.g. "zj" against the
 * initials "zzj"). Returns the first matched index and the span covering all
 * matched characters, or null when the query is not a subsequence.
 */
export function orderedSubsequenceSpan(text: string, query: string): { first: number; span: number } | null {
  if (!query) return null;
  let from = 0;
  let first = -1;
  let last = -1;
  for (const char of query) {
    const position = text.indexOf(char, from);
    if (position < 0) return null;
    if (first < 0) first = position;
    last = position;
    from = position + 1;
  }
  return { first, span: last - first + 1 };
}

/**
 * Generic matcher for small pick-lists (grid condition editor, ...):
 * prefix > pinyin-initials prefix > substring > pinyin-initials subsequence.
 * Returns -1 for no match; scores are per-tier constants so same-tier
 * candidates keep their original order. An empty query matches everything.
 */
export function pinyinAwareMatchScore(candidate: string, query: string): number {
  const text = candidate.toLowerCase();
  const normalized = query.trim().toLowerCase();
  if (!normalized) return 0;
  if (text.startsWith(normalized)) return 400;
  const asciiQuery = /^[a-z0-9]+$/.test(normalized);
  const han = containsHan(text);
  if (asciiQuery && han && pinyinFirstLetters(text).startsWith(normalized)) return 300;
  if (text.includes(normalized)) return 200;
  if (asciiQuery && han) {
    const subsequence = orderedSubsequenceSpan(pinyinFirstLetters(text), normalized);
    if (subsequence) return 100;
  }
  return -1;
}

preloadPinyinWhenIdle();
