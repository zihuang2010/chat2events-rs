import { describe, expect, it } from "vitest";
import {
  addDays,
  dayOf,
  formatDuration,
  formatDateTime,
  formatPercent,
  parseDateTime,
  weekdayOf,
} from "./format";

describe("时间解析", () => {
  it("按 UTC+8 解析并回写数据库墙钟，与浏览器时区无关", () => {
    const d = parseDateTime("2026-08-25 17:00:54");
    expect(d.toISOString()).toBe("2026-08-25T09:00:54.000Z");
    expect(formatDateTime(d)).toBe("2026-08-25 17:00:54");
  });

  it("浏览器所在地区进入夏令时不改变首响时长", () => {
    const start = parseDateTime("2026-03-08 01:30:00");
    const end = parseDateTime("2026-03-08 03:30:00");
    expect((end.getTime() - start.getTime()) / 1000).toBe(7200);
  });

  it("dayOf 取归属日", () => {
    expect(dayOf("2026-08-25 17:00:54")).toBe("2026-08-25");
  });

  it("addDays 跨月", () => {
    expect(addDays("2026-08-31", 1)).toBe("2026-09-01");
    expect(addDays("2026-09-01", -1)).toBe("2026-08-31");
  });

  it("weekdayOf", () => {
    expect(weekdayOf("2026-08-25")).toMatch(/^周/);
  });
});

describe("时长格式化", () => {
  it("null 保持 null，绝不折成 0 秒", () => {
    expect(formatDuration(null)).toBeNull();
  });

  it("按量级换单位", () => {
    expect(formatDuration(45)).toBe("45 秒");
    expect(formatDuration(300)).toBe("5 分");
    expect(formatDuration(3600)).toBe("1 小时");
    expect(formatDuration(3600 + 51 * 60)).toBe("1 小时 51 分");
    expect(formatDuration(30 * 3600)).toBe("1 天 6 小时");
  });
  it("四舍五入跨分钟、小时和天边界时正确进位", () => {
    expect(formatDuration(59.5)).toBe("1 分");
    expect(formatDuration(3590)).toBe("1 小时");
    expect(formatDuration(7190)).toBe("2 小时");
    expect(formatDuration(86390)).toBe("1 天 0 小时");
  });
});

describe("比率格式化", () => {
  it("null 保持 null", () => {
    expect(formatPercent(null)).toBeNull();
  });
  it("比率用百分比", () => {
    expect(formatPercent(0.308)).toBe("30.8%");
  });
});
