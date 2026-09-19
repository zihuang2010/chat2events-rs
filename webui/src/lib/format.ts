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

/** `b - a` 的自然日数。两端都是 UTC+8 的 00:00，差值必是整数。 */
export const daysBetween = (a: string, b: string): number =>
  Math.round((parseDateTime(b).getTime() - parseDateTime(a).getTime()) / 86_400_000);

/**
 * 当轮跑批跑完的钟点。**旋钮，不是常数** —— 定时器 03:15 起跑
 * （`docs/deploy.md`「定时跑」的 `OnCalendar`），跑完才有当轮数据。
 * 在那之前库里最新的一天仍是 T-3，那是**等待**不是滞后，报出来就是每天凌晨
 * 一句假警报。改跑批时间或跑批明显变慢，要一起改这里。
 */
const BATCH_DONE_HOUR = 8;

/**
 * 数据截至日的说明。**T+2 跑批下今天和昨天永远没有数据，那不是故障** ——
 * 页面不写出来的话，用户每天打开看到的都是「少了两天」，只能靠猜是数据坏了
 * 还是本来如此。真正该看一眼的是落后于 T-2 的那种：有一轮没跑成或还没跑。
 *
 * `now` 是 UTC+8 墙钟的 `YYYY-MM-DD HH:mm:ss` —— **要钟点不只要日期**，
 * 判「今轮跑完没」靠它。落后不到两天（补跑过近几天）没有话可说，返回 null。
 */
export function freshnessNote(
  lastDay: string,
  now = formatDateTime(new Date()),
): { text: string; stale: boolean } | null {
  const lag = daysBetween(lastDay, dayOf(now));
  if (lag <= 1) return null;
  if (lag === 2) return { text: "T+2 跑批，今天与昨天尚未覆盖", stale: false };
  if (lag === 3 && Number(now.slice(11, 13)) < BATCH_DONE_HOUR) {
    return { text: "T+2 跑批，今轮尚未跑完", stale: false };
  }
  return { text: `落后预期 ${lag - 2} 天`, stale: true };
}

/**
 * URL 上的日期：形如 YYYY-MM-DD **且真实存在**（2 月 30 号不算）才采信，否则按未指定处理。
 *
 * 取数路径也用它：那边不再自己算窗口（后端 `Period::bounds` 一直在算），
 * 但仍要挡住 URL 里的垃圾 —— 原样透传会把「用户手抖改了地址栏」变成一个 400 错误页。
 */
export function validDate(value: string | null | undefined): string | null {
  return value && /^\d{4}-\d{2}-\d{2}$/.test(value) && addDays(value, 0) === value ? value : null;
}

/** 默认最近七天；显式日期保留，非法 URL 日期按未指定处理。days 由 meta 保证非空。 */
export function windowBounds(days: readonly string[], from?: string | null, to?: string | null) {
  const valid = validDate;
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
