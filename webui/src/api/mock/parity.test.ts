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
import { buildTaxonomyIndex } from "@/domain/metrics";
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
