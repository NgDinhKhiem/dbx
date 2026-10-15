/**
 * Per-connection history of executed console requests, kept in localStorage.
 * Only the request text is stored — never response bodies.
 */

export const CONSOLE_HISTORY_LIMIT = 50;
/** Requests larger than this (e.g. big `_bulk` payloads) keep only their request line. */
export const CONSOLE_HISTORY_MAX_TEXT_LENGTH = 32 * 1024;
const STORAGE_PREFIX = "dbx.esConsole.history.v1.";

export interface ConsoleHistoryEntry {
  /** Request line, e.g. `GET _cat/indices?v`. */
  label: string;
  /** Request text to re-insert (may be only the request line when too large). */
  text: string;
  executedAt: number;
  /** True when the body was dropped because it was too large to keep. */
  truncated?: boolean;
}

interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

function defaultStorage(): StorageLike | undefined {
  try {
    return globalThis.localStorage ?? undefined;
  } catch {
    return undefined;
  }
}

export function consoleHistoryStorageKey(connectionId: string): string {
  return `${STORAGE_PREFIX}${connectionId}`;
}

function isHistoryEntry(value: unknown): value is ConsoleHistoryEntry {
  if (!value || typeof value !== "object") return false;
  const entry = value as Partial<ConsoleHistoryEntry>;
  return typeof entry.label === "string" && typeof entry.text === "string" && typeof entry.executedAt === "number";
}

export function loadConsoleHistory(connectionId: string, storage: StorageLike | undefined = defaultStorage()): ConsoleHistoryEntry[] {
  if (!storage || !connectionId) return [];
  try {
    const raw = storage.getItem(consoleHistoryStorageKey(connectionId));
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.filter(isHistoryEntry).slice(0, CONSOLE_HISTORY_LIMIT) : [];
  } catch {
    return [];
  }
}

export function saveConsoleHistory(connectionId: string, entries: readonly ConsoleHistoryEntry[], storage: StorageLike | undefined = defaultStorage()): void {
  if (!storage || !connectionId) return;
  try {
    storage.setItem(consoleHistoryStorageKey(connectionId), JSON.stringify(entries.slice(0, CONSOLE_HISTORY_LIMIT)));
  } catch {
    // Storage full or unavailable: history is a convenience, never fatal.
  }
}

export function createConsoleHistoryEntry(label: string, text: string, executedAt = Date.now()): ConsoleHistoryEntry {
  const trimmed = text.trim();
  if (trimmed.length > CONSOLE_HISTORY_MAX_TEXT_LENGTH) return { label, text: label, executedAt, truncated: true };
  return { label, text: trimmed, executedAt };
}

/** Newest first; re-running an identical request moves it to the top instead of duplicating it. */
export function addConsoleHistoryEntries(entries: readonly ConsoleHistoryEntry[], added: readonly ConsoleHistoryEntry[]): ConsoleHistoryEntry[] {
  let next = [...entries];
  for (const entry of added) {
    next = [entry, ...next.filter((existing) => existing.text !== entry.text)];
  }
  return next.slice(0, CONSOLE_HISTORY_LIMIT);
}

export function recordConsoleHistory(connectionId: string, added: readonly ConsoleHistoryEntry[], storage: StorageLike | undefined = defaultStorage()): ConsoleHistoryEntry[] {
  const next = addConsoleHistoryEntries(loadConsoleHistory(connectionId, storage), added);
  saveConsoleHistory(connectionId, next, storage);
  return next;
}

export function clearConsoleHistory(connectionId: string, storage: StorageLike | undefined = defaultStorage()): void {
  saveConsoleHistory(connectionId, [], storage);
}
