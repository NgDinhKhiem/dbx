import type { DiscoverTimeRange } from "./types";

export interface QuickRange {
  /** i18n key suffix under `esDiscover.quickRanges`. */
  key: string;
  from: string;
  to: string;
}

export const QUICK_RANGES: readonly QuickRange[] = [
  { key: "last15m", from: "now-15m", to: "now" },
  { key: "last1h", from: "now-1h", to: "now" },
  { key: "last4h", from: "now-4h", to: "now" },
  { key: "last24h", from: "now-24h", to: "now" },
  { key: "last7d", from: "now-7d", to: "now" },
  { key: "last30d", from: "now-30d", to: "now" },
  { key: "last90d", from: "now-90d", to: "now" },
  { key: "last1y", from: "now-1y", to: "now" },
];

export const DEFAULT_TIME_RANGE: DiscoverTimeRange = { from: "now-15m", to: "now" };

const UNIT_MS: Record<string, number> = {
  s: 1000,
  m: 60_000,
  h: 3_600_000,
  d: 86_400_000,
  w: 7 * 86_400_000,
};

function addUnits(date: Date, amount: number, unit: string): Date {
  const result = new Date(date.getTime());
  if (unit === "M") {
    result.setMonth(result.getMonth() + amount);
  } else if (unit === "y") {
    result.setFullYear(result.getFullYear() + amount);
  } else {
    result.setTime(result.getTime() + amount * UNIT_MS[unit]);
  }
  return result;
}

function roundDate(date: Date, unit: string, roundUp: boolean): Date {
  const result = new Date(date.getTime());
  switch (unit) {
    case "y":
      result.setMonth(0, 1);
      result.setHours(0, 0, 0, 0);
      break;
    case "M":
      result.setDate(1);
      result.setHours(0, 0, 0, 0);
      break;
    case "w": {
      const day = (result.getDay() + 6) % 7; // Monday-based week
      result.setDate(result.getDate() - day);
      result.setHours(0, 0, 0, 0);
      break;
    }
    case "d":
      result.setHours(0, 0, 0, 0);
      break;
    case "h":
      result.setMinutes(0, 0, 0);
      break;
    case "m":
      result.setSeconds(0, 0);
      break;
    case "s":
      result.setMilliseconds(0);
      break;
  }
  if (roundUp) return new Date(addUnits(result, 1, unit).getTime() - 1);
  return result;
}

/**
 * Parse a date-math expression (`now`, `now-15m`, `now-1d/d`) or an absolute
 * date. `roundUp` rounds `/unit` to the end of the unit (used for range ends).
 * Returns null for unparseable input.
 */
export function parseDateMath(expression: string, now: Date = new Date(), roundUp = false): Date | null {
  const text = expression.trim();
  if (!text) return null;
  if (text.startsWith("now")) {
    let date = new Date(now.getTime());
    let rest = text.slice(3);
    const pattern = /^([+-])(\d+)([smhdwMy])|^\/([smhdwMy])/;
    while (rest.length > 0) {
      const match = pattern.exec(rest);
      if (!match) return null;
      if (match[4]) {
        date = roundDate(date, match[4], roundUp);
      } else {
        const amount = Number(match[2]) * (match[1] === "-" ? -1 : 1);
        date = addUnits(date, amount, match[3]);
      }
      rest = rest.slice(match[0].length);
    }
    return date;
  }
  if (/^\d{10,}$/.test(text)) return new Date(Number(text));
  const parsed = Date.parse(text);
  return Number.isNaN(parsed) ? null : new Date(parsed);
}

export interface ResolvedTimeRange {
  from: Date;
  to: Date;
}

export function resolveTimeRange(range: DiscoverTimeRange, now: Date = new Date()): ResolvedTimeRange | null {
  const from = parseDateMath(range.from, now, false);
  const to = parseDateMath(range.to, now, true);
  if (!from || !to || from.getTime() > to.getTime()) return null;
  return { from, to };
}

export function isRelativeDate(expression: string): boolean {
  return expression.trim().startsWith("now");
}

export function quickRangeFor(range: DiscoverTimeRange): QuickRange | undefined {
  return QUICK_RANGES.find((candidate) => candidate.from === range.from && candidate.to === range.to);
}

interface NiceInterval {
  expression: string;
  ms: number;
}

const NICE_INTERVALS: readonly NiceInterval[] = [
  ["1s", 1000],
  ["2s", 2000],
  ["5s", 5000],
  ["10s", 10_000],
  ["15s", 15_000],
  ["30s", 30_000],
  ["1m", 60_000],
  ["2m", 120_000],
  ["5m", 300_000],
  ["10m", 600_000],
  ["15m", 900_000],
  ["30m", 1_800_000],
  ["1h", 3_600_000],
  ["2h", 7_200_000],
  ["3h", 10_800_000],
  ["6h", 21_600_000],
  ["12h", 43_200_000],
  ["1d", 86_400_000],
  ["2d", 172_800_000],
  ["7d", 604_800_000],
  ["14d", 1_209_600_000],
  ["30d", 2_592_000_000],
  ["90d", 7_776_000_000],
  ["365d", 31_536_000_000],
].map(([expression, ms]) => ({ expression: expression as string, ms: ms as number }));

export const TARGET_BUCKETS = 50;
const MAX_BUCKETS = 100;

/**
 * Pick a "nice" `fixed_interval` for a histogram over `spanMs` that lands as
 * close as possible to ~50 buckets (and never more than 100).
 */
export function autoInterval(spanMs: number): NiceInterval {
  if (!(spanMs > 0)) return NICE_INTERVALS[0];
  let best = NICE_INTERVALS[NICE_INTERVALS.length - 1];
  let bestScore = Number.POSITIVE_INFINITY;
  for (const interval of NICE_INTERVALS) {
    const buckets = spanMs / interval.ms;
    if (buckets > MAX_BUCKETS) continue;
    const score = Math.abs(Math.log(buckets / TARGET_BUCKETS));
    if (score < bestScore) {
      best = interval;
      bestScore = score;
    }
  }
  return best;
}

function pad(value: number, length = 2): string {
  return String(value).padStart(length, "0");
}

/** `YYYY-MM-DD HH:mm:ss.SSS` in local time. */
export function formatDateTime(date: Date, withMillis = true): string {
  const base = `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
  return withMillis ? `${base}.${pad(date.getMilliseconds(), 3)}` : base;
}

/** Axis label format adapted to the interval size. */
export function formatBucketLabel(timestamp: number, intervalMs: number): string {
  const date = new Date(timestamp);
  if (intervalMs >= 86_400_000) return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
  if (intervalMs >= 60_000) return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}
