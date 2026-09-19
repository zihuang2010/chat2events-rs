/**
 * 口径对拍（前端这一侧）—— **同一批输入，必须和后端 SQL 算出同一组数字。**
 *
 * ⚠️ 后端 `src/web/query.rs` 里写着「落地时必须有一条测试拿同一批数据对拍两边」，
 * 这是那条的前端一半；另一半是 `mysql_summary_matches_the_frontend_definitions`。
 * 两边读同一个 `domain/parity-vectors.json`，各自断言等于同一组 `expected`。
 *
 * 口径分家是**静默**的：页面照样显示一个看起来合理的数字，没有人会发现。
 * 这条测试钉的不是「结果对不对」，是「两个实现有没有开始分家」。
 */
import { expect, it } from "vitest";
import vectors from "@/domain/parity-vectors.json";
import { mockSummary } from "./aggregate";
import { buildTaxonomyIndex, quantile } from "@/domain/metrics";
import type { EventRow, GroupDailyRow } from "@/domain/schemas";

it("概览 KPI 与后端 SQL 逐字段相等", () => {
  const actual = mockSummary(
    vectors.events as unknown as EventRow[],
    vectors.groupDaily as unknown as GroupDailyRow[],
    buildTaxonomyIndex([], "v1"),
    { from: vectors.window.from, to: vectors.window.to, slaSec: vectors.slaSec },
  );
  expect(actual).toEqual(vectors.expected);
});

/**
 * 分位数口径对拍 —— **三份实现读同一组向量**。
 *
 * 另外两份：跑批落库的 `stage::metrics::pct`（`stage/metrics/tests.rs`，离线跑）·
 * 只读取数的 `SUMMARY_QUANTILES`（`web/tests.rs` 的 `mysql_` 测试，跑真 SQL）。
 *
 * ⚠️ 上面那条 `expected` 钉的是「从 events 算出来的 p50/p90」，而
 * `groupDaily.first_reply_p50_sec` 在那组向量里是**输入** —— 所以它碰不到 `pct`。
 * 这一组才是唯一能同时压住三份实现的东西。
 */
it("分位数定义三份实现一致", () => {
  expect(vectors.quantileCases.length).toBeGreaterThan(0);
  for (const c of vectors.quantileCases) {
    const secs = c.secs as number[];
    expect(secs).toEqual([...secs].sort((a, b) => a - b));
    expect(quantile(secs, 0.5)).toBe(c.p50);
    expect(quantile(secs, 0.9)).toBe(c.p90);
  }
});
