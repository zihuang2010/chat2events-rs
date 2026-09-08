/**
 * 数据库 DATETIME 表示 UTC+8 墙钟，解析与格式化不依赖浏览器的本地时区。
 */

const WEEKDAY = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"] as const;
const pad2 = (n: number) => String(n).padStart(2, "0");

export function parseDateTime(s: string): Date {
  return new Date(`${s.includes(" ") ? s.replace(" ", "T") : `${s}T00:00:00`}+08:00`);
}

export function formatDateTime(d: Date): string {
  const wall = new Date(d.getTime() + 8 * 3600_000);
  return `${wall.getUTCFullYear()}-${pad2(wall.getUTCMonth() + 1)}-${pad2(wall.getUTCDate())} ${pad2(wall.getUTCHours())}:${pad2(wall.getUTCMinutes())}:${pad2(wall.getUTCSeconds())}`;
}

export const dayOf = (dateTime: string): string => dateTime.slice(0, 10);

export function weekdayOf(date: string): string {
  const [y = "1970", m = "01", d = "01"] = date.split("-");
  return WEEKDAY[new Date(Date.UTC(Number(y), Number(m) - 1, Number(d))).getUTCDay()] ?? "";
}

export function addDays(date: string, delta: number): string {
  const [y = "1970", m = "01", d = "01"] = date.split("-");
  return new Date(Date.UTC(Number(y), Number(m) - 1, Number(d) + delta)).toISOString().slice(0, 10);
}

/** 默认最近七天；显式日期保留，非法 URL 日期按未指定处理。days 由 meta 保证非空。 */
export function windowBounds(days: readonly string[], from?: string | null, to?: string | null) {
  const valid = (value: string | null | undefined) =>
    value && /^\d{4}-\d{2}-\d{2}$/.test(value) && addDays(value, 0) === value ? value : null;
  const end = valid(to) ?? days.at(-1)!;
  const recent = addDays(end, -6);
  const start = valid(from) ?? (recent < days[0]! ? days[0]! : recent);
  return { from: start, to: end < start ? start : end };
}

/** 时长。**null 表示没有这个值**，由调用方决定怎么显示，这里绝不替它编 0。 */
export function formatDuration(sec: number | null): string | null {
  if (sec === null) return null;
  if (Math.round(sec) < 60) return `${Math.round(sec)} 秒`;
  const totalMinutes = Math.round(sec / 60);
  if (totalMinutes < 60) return `${totalMinutes} 分`;
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  if (hours < 24) return minutes ? `${hours} 小时 ${minutes} 分` : `${hours} 小时`;
  return `${Math.floor(hours / 24)} 天 ${hours % 24} 小时`;
}

/** 紧凑时长，给窄列用：不带空格，天级只保留到小时。 */
export function formatDurationCompact(sec: number | null): string | null {
  if (sec === null) return null;
  if (sec < 60) return `${Math.round(sec)}秒`;
  if (sec < 3600) return `${Math.round(sec / 60)}分`;
  const hours = Math.floor(sec / 3600);
  if (hours < 24) return `${hours}时`;
  return `${Math.floor(hours / 24)}天${hours % 24}时`;
}

export function formatPercent(v: number | null, digits = 1): string | null {
  return v === null ? null : `${(v * 100).toFixed(digits)}%`;
}

export const shortId = (id: string, n = 10): string => id.slice(0, n);

export const formatInt = (n: number): string => n.toLocaleString("zh-CN");
