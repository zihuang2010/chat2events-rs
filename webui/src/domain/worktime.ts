/**
 * 工作时段口径 `[08:30, 21:00)` —— 前端这一份。
 *
 * 后端那份在 `src/worktime.rs`，**三个常量必须与它逐字相同**。跨语言，编译器管不到，
 * 只能靠这条注释互指。口径漂了不会有任何东西报错：秒数只是悄悄变小一点。
 *
 * 用它的两处：
 *   - `domain/metrics.ts` 的 `decorate()` 算 `firstReplySec`（首响 / 超时 / 分位数全靠它）
 *   - `test/mock/generator.ts` 造 `followup_wait_max_sec`
 *
 * ⚠️ **每天都算工作日**：周末与节假日不扣。扣它们要一份工作日历，今天没有。
 *
 * ⚠️ 入参是库里的 `"YYYY-MM-DD HH:MM:SS"` 原样字符串，**不经过 Date**。
 * 库里的 DATETIME 不做时区转换、取值即业务本地时间，所以直接按墙钟切开算；
 * 绕一圈 `Date` 只会把浏览器时区拖进来。
 */
export const WORK_OPEN_SEC = 8 * 3600 + 30 * 60;
export const WORK_CLOSE_SEC = 21 * 3600;
export const WORK_DAY_SEC = WORK_CLOSE_SEC - WORK_OPEN_SEC;

/** `worktime::elapsed` 的对应物 —— 原点任意，有意义的只有两点之差。 */
export function workElapsedSec(at: string): number {
  const days = Math.floor(Date.parse(`${at.slice(0, 10)}T00:00:00Z`) / 86_400_000);
  const [h, m, s] = at.slice(11).split(":").map(Number);
  const secs = (h ?? 0) * 3600 + (m ?? 0) * 60 + (s ?? 0);
  return days * WORK_DAY_SEC + Math.min(Math.max(secs, WORK_OPEN_SEC), WORK_CLOSE_SEC);
}

/** `worktime::between` 的对应物 —— 下界 0（上游时间戳乱序不是「负的响应时长」）。 */
export function workSecsBetween(from: string, to: string): number {
  return Math.max(0, workElapsedSec(to) - workElapsedSec(from));
}
