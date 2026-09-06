import { describe, expect, it } from "vitest";
import { applyPatch, parseFilters } from "./useFilters";

describe("筛选状态的 URL 编码", () => {
  it("日期控件和指标共用有效日期，不渲染 Invalid Date", () => {
    expect(parseFilters(new URLSearchParams("from=2026-02-30&to=invalid"))).toMatchObject({
      from: null,
      to: null,
    });
    expect(parseFilters(new URLSearchParams("from=2026-08-31&to=2026-08-25"))).toMatchObject({
      from: "2026-08-31",
      to: "2026-08-31",
    });
  });
  it("空查询串解析出全空条件与默认值", () => {
    const f = parseFilters(new URLSearchParams());
    expect(f.room).toBeNull();
    expect(f.status).toBeNull();
    expect(f.overdueOnly).toBeNull();
    expect(f.slaSec).toBe(1800);
    expect(f.page).toBe(1);
    expect(f.pageSize).toBe(20);
  });

  it("默认值不落到 URL 上", () => {
    const sp = applyPatch(new URLSearchParams(), {
      slaSec: 1800,
      page: 1,
      pageSize: 20,
      rank: "events",
    });
    expect(sp.toString()).toBe("");
  });

  it("非默认值写入，null 清除", () => {
    let sp = applyPatch(new URLSearchParams(), { room: "R1", slaSec: 3600 });
    expect(sp.get("room")).toBe("R1");
    expect(sp.get("sla")).toBe("3600");
    sp = applyPatch(sp, { room: null });
    expect(sp.get("room")).toBeNull();
  });

  it("显式分页链接保持原有条数，切回 20 条移除 size", () => {
    for (const size of [25, 50, 100, 200]) {
      const sp = applyPatch(new URLSearchParams(), { pageSize: size });
      expect(parseFilters(sp).pageSize).toBe(size);
      expect(sp.get("size")).toBe(String(size));
      expect(applyPatch(sp, { pageSize: 20 }).has("size")).toBe(false);
    }
  });

  it("布尔筛选用 1/0，且 0 与「未设置」不同", () => {
    const on = applyPatch(new URLSearchParams(), { overdueOnly: true });
    const off = applyPatch(new URLSearchParams(), { overdueOnly: false });
    expect(parseFilters(on).overdueOnly).toBe(true);
    expect(parseFilters(off).overdueOnly).toBe(false);
    expect(parseFilters(new URLSearchParams()).overdueOnly).toBeNull();
  });

  it("改任何筛选条件都把分页复位", () => {
    const sp = applyPatch(new URLSearchParams("page=7"), { room: "R1" });
    expect(sp.get("page")).toBeNull();
  });

  it("显式翻页时保留页码", () => {
    const sp = applyPatch(new URLSearchParams("room=R1"), { page: 3 });
    expect(sp.get("page")).toBe("3");
    expect(sp.get("room")).toBe("R1");
  });

  it("非法状态值被丢弃，不会把页面筛成空", () => {
    expect(parseFilters(new URLSearchParams("status=乱写")).status).toBeNull();
  });

  it("非法页码回落到 1，不会出现负数页", () => {
    expect(parseFilters(new URLSearchParams("page=-3")).page).toBe(1);
    expect(parseFilters(new URLSearchParams("page=abc")).page).toBe(1);
    expect(parseFilters(new URLSearchParams("page=0.5&size=0.5&drawer=1.5"))).toMatchObject({
      page: 1,
      pageSize: 20,
      drawer: null,
    });
    expect(parseFilters(new URLSearchParams("overdue=invalid")).overdueOnly).toBeNull();
  });
});
