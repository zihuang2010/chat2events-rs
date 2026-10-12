import { expect, it } from "vitest";
import type { RoomRow } from "@/domain/metrics";
import type { RoomCategoryAgg } from "@/domain/schemas";
import type { WorkbookSheet } from "@/features/export/workbook";
import { parentGroups } from "@/features/filters/useAnalytics";
import { parseFilters } from "@/features/filters/useFilters";
import { buildMockDataset } from "@/test/mock/generator";
import { column, headers, record } from "@/test/workbook";
import { roomPivotSheet, roomSheet, type RoomPivotView, type RoomSheetView } from "./exportRooms";

const raw = buildMockDataset();
const { meta } = raw;
const parents = parentGroups({ ...raw, taxIndex: new Map(), source: "api", loadedAt: 0 });
const view: RoomSheetView = {
  days: ["2026-10-01", "2026-10-02", "2026-10-03"],
  lastDay: "2026-10-03",
  roomLabel: (id) => `群${id}`,
  agentLabel: (id) => `客服${id}`,
  roomMerchant: (id) => (id === "r1" ? "一号店" : null),
  roomMerchantGroup: (id) => (id === "r1" ? "华东" : null),
  roomManager: (id) => (id === "r1" ? "张三" : null),
};
const pivotView = (q: RoomPivotView["q"] = {}): RoomPivotView => ({ ...view, parents, q });
// 2026-10-09 18:30 UTC+8
const now = new Date("2026-10-09T10:30:00Z");

function row(patch: Partial<RoomRow> & Pick<RoomRow, "key">): RoomRow {
  return {
    label: `群${patch.key}`,
    events: 0,
    merchant: 0,
    unreplied: 0,
    unrepliedRate: null,
    p50: null,
    p90: null,
    overdue: 0,
    overdueRate: null,
    backlog: 0,
    msgs: null,
    senders: null,
    failedDays: 0,
    pendingLabels: 0,
    failedLabels: 0,
    missingDays: 0,
    unknownDays: 0,
    totalDays: 3,
    series: [],
    topLevel1: [],
    ...patch,
  };
}

/** 宽表第 i 行里「事件量」与「数据完整性」之间的格子：各分类列 + 未归类 + 打标未完成。 */
const tallies = (sheet: WorkbookSheet, i: number) =>
  sheet.rows[i]!.slice(headers(sheet).indexOf("事件量") + 1, headers(sheet).indexOf("数据完整性"));

it("describes the metrics sheet with rows by event count and gaps left empty", () => {
  const rows = [
    row({ key: "r0", events: null, merchant: null, unreplied: null, failedDays: 3 }),
    row({ key: "r2", events: 2 }),
    row({
      key: "r1",
      events: 5,
      merchant: 4,
      unreplied: 1,
      unrepliedRate: 0.25,
      p50: 90,
      p90: 3600,
      overdueRate: 0.5,
      msgs: 120,
    }),
  ];
  const sheet = roomSheet(rows, view, meta, parseFilters(new URLSearchParams()), now);

  expect(sheet.name).toBe("群维度指标");
  expect(sheet.stickyColumns).toBeUndefined();
  expect(headers(sheet)).toEqual([
    "群名称",
    "商家名称",
    "商家分组",
    "业务经理",
    "消息总量",
    "事件量",
    "商家发起",
    "首响 P50",
    "首响 P90",
    "无响应",
    "无响应率",
    "超时率",
    "数据完整性",
  ]);
  expect(column(sheet, "群名称")).toEqual(["群r1", "群r2", "群r0"]);
  expect(record(sheet, 0)).toEqual({
    群名称: "群r1",
    商家名称: "一号店",
    商家分组: "华东",
    业务经理: "张三",
    消息总量: 120,
    事件量: 5,
    商家发起: 4,
    "首响 P50": { value: 90 / 86_400, format: "[h]:mm:ss" },
    "首响 P90": { value: 3600 / 86_400, format: "[h]:mm:ss" },
    无响应: 1,
    无响应率: { value: 0.25, format: "0.0%" },
    超时率: { value: 0.5, format: "0.0%" },
    数据完整性: "3 日完整",
  });
  // 抽取整段失败：事件级是空格子，不是 0（承重不变量 4）
  expect(record(sheet, 2)).toEqual({
    群名称: "群r0",
    商家名称: null,
    商家分组: null,
    业务经理: null,
    消息总量: null,
    事件量: null,
    商家发起: null,
    "首响 P50": null,
    "首响 P90": null,
    无响应: null,
    无响应率: null,
    超时率: null,
    数据完整性: "3 / 3 日失败",
  });
});

it("summary always states dates and threshold, and only the filters in effect", () => {
  const plain = roomSheet([], view, meta, parseFilters(new URLSearchParams()), now).caption;
  expect(plain).toBe(
    "日期 2026-10-01 ~ 2026-10-03（3 天，UTC+8）｜首响阈值 30 分钟｜共 0 个群｜导出于 2026-10-09 18:30",
  );

  const filtered = roomSheet(
    [],
    view,
    meta,
    parseFilters(
      new URLSearchParams("sla=3600&group=华东&q= 退款 &l2=urge_accept&status=unreplied&overdue=0"),
    ),
    now,
  ).caption;
  expect(filtered).toBe(
    "日期 2026-10-01 ~ 2026-10-03（3 天，UTC+8）｜首响阈值 1 小时｜商家分组：华东｜事件摘要：退款｜二级分类：履约催促 / 催接单｜状态：无响应｜超时：仅未超时｜共 0 个群｜导出于 2026-10-09 18:30",
  );
});

it("pivot sheets keep the metrics row order, list every taxonomy column, and keep untyped and unlabeled apart", () => {
  const rows = [
    row({ key: "r0", events: null, failedDays: 3 }),
    row({ key: "r3", events: 0 }),
    row({ key: "r2", events: 2 }),
    row({ key: "r1", events: 6 }),
  ];
  const aggs: RoomCategoryAgg[] = [
    {
      roomid: "r1",
      counts: { urge_accept: 1, urge_dispatch: 1, __untyped__: 1, retired_code: 1 },
      unlabeled: 2,
    },
    { roomid: "r2", counts: { tech_reassign: 2 }, unlabeled: 0 },
  ];
  const filters = parseFilters(new URLSearchParams());
  const l1 = roomPivotSheet("level1", rows, aggs, pivotView(), meta, filters, now);
  const l2 = roomPivotSheet("level2", rows, aggs, pivotView(), meta, filters, now);

  expect(l1.name).toBe("群×一级分类");
  expect(l2.name).toBe("群×二级分类");
  for (const sheet of [l1, l2]) {
    // 横向滚动时冻住首列「群名称」
    expect(sheet.stickyColumns).toBe(1);
    expect(sheet.caption).toContain("共 4 个群");
    expect(headers(sheet).slice(0, 5)).toEqual([
      "群名称",
      "商家名称",
      "商家分组",
      "业务经理",
      "事件量",
    ]);
    expect(headers(sheet).slice(-3)).toEqual(["未归类", "打标未完成", "数据完整性"]);
    // 与第一个 sheet 同序（事件量倒序，缺失最后），0 事件的群也占一行
    expect(column(sheet, "群名称")).toEqual(["群r1", "群r2", "群r3", "群r0"]);
    // 每行：各分类列 + 未归类 + 打标未完成 = 事件量
    expect((tallies(sheet, 0) as number[]).reduce((a, b) => a + b, 0)).toBe(6);
    // 真 0 事件写 0；抽取全失败整行留空（承重不变量 4）
    expect(tallies(sheet, 2).every((v) => v === 0)).toBe(true);
    expect(record(sheet, 3).事件量).toBeNull();
    expect(tallies(sheet, 3).every((v) => v === null)).toBe(true);
    expect(record(sheet, 3).数据完整性).toBe("3 / 3 日失败");
  }

  // 一级按中文名排序，全量 9 列
  expect(headers(l1).slice(5, -3)).toEqual([
    "订单变更",
    "费用结算",
    "服务售后",
    "履约催促",
    "师傅调度",
    "损坏赔付",
    "无明确诉求",
    "信息异常",
    "业务查询",
  ]);
  expect(record(l1, 0)).toMatchObject({
    群名称: "群r1",
    商家名称: "一号店",
    事件量: 6,
    履约催促: 2,
    师傅调度: 0,
    未归类: 2,
    打标未完成: 2,
    数据完整性: "3 日完整",
  });
  expect(record(l1, 1)).toMatchObject({ 师傅调度: 2, 履约催促: 0 });

  // 二级全量 43 列，按所属一级聚在一起、组内按中文名
  const l2Headers = headers(l2).slice(5, -3);
  expect(l2Headers).toHaveLength(meta.taxonomy.length);
  expect(l2Headers.slice(0, 8)).toEqual([
    "挂起订单",
    "恢复服务",
    "加单",
    "减单",
    "取消服务",
    "修改订单资料",
    "修改服务内容",
    "修改履约要求",
  ]);
  expect(record(l2, 0)).toMatchObject({
    催接单: 1,
    催派单: 1,
    改派师傅: 0,
    未归类: 2,
    打标未完成: 2,
  });
  expect(record(l2, 1)).toMatchObject({ 改派师傅: 2, 催接单: 0 });
});

it("pivot sheets leave columns outside the category filter empty instead of 0", () => {
  const rows = [row({ key: "r1", events: 1 })];
  const filters = parseFilters(new URLSearchParams());
  const sheets = (q: RoomPivotView["q"], counts: Record<string, number>) => {
    const input = [{ roomid: "r1", counts, unlabeled: 0 }];
    return [
      record(roomPivotSheet("level1", rows, input, pivotView(q), meta, filters, now), 0),
      record(roomPivotSheet("level2", rows, input, pivotView(q), meta, filters, now), 0),
    ];
  };

  // 筛了二级「催接单」：只有它和它的一级有数；未归类、打标未完成都没参与计数
  const [l1, l2] = sheets({ types: ["urge_accept"] }, { urge_accept: 1 });
  expect(l1).toMatchObject({
    事件量: 1,
    履约催促: 1,
    师傅调度: null,
    未归类: null,
    打标未完成: null,
  });
  expect(l2).toMatchObject({ 催接单: 1, 催派单: null, 未归类: null, 打标未完成: null });

  // 筛了一级「未归类」（排除式）：只有未归类一列有数
  const known = meta.taxonomy.map((t) => t.type_id);
  const [u1, u2] = sheets({ typesExclude: known }, { retired_code: 1 });
  expect(u1).toMatchObject({ 履约催促: null, 未归类: 1, 打标未完成: null });
  expect(u2).toMatchObject({ 催接单: null, 未归类: 1, 打标未完成: null });

  // 筛了二级「归不上去」（__untyped__ 不在词表里）：同样落在未归类
  const [g1] = sheets({ types: ["__untyped__"] }, { __untyped__: 1 });
  expect(g1).toMatchObject({ 履约催促: null, 未归类: 1, 打标未完成: null });

  // 两个都给：与后端一样取交集，被排除的那个不算在范围内
  const [b1, b2] = sheets(
    { types: ["urge_accept", "urge_dispatch"], typesExclude: ["urge_dispatch"] },
    { urge_accept: 1 },
  );
  expect(b1).toMatchObject({ 履约催促: 1, 未归类: null, 打标未完成: null });
  expect(b2).toMatchObject({ 催接单: 1, 催派单: null, 未归类: null });
});

it("pivot sheets refuse to export when category tallies no longer add up to the event count", () => {
  const filters = parseFilters(new URLSearchParams());
  const build = (rows: RoomRow[], aggs: RoomCategoryAgg[]) => () =>
    roomPivotSheet("level1", rows, aggs, pivotView(), meta, filters, now);

  // 群指标是早先取的，分类计数是现取的：中间数据变了，两边对不上
  expect(
    build(
      [row({ key: "r1", events: 3 })],
      [{ roomid: "r1", counts: { urge_accept: 2 }, unlabeled: 0 }],
    ),
  ).toThrow("数据刚有更新（群「群r1」分类合计 2，事件量 3），请刷新页面后重新导出");
  expect(build([row({ key: "r1", events: 2 })], [])).toThrow("分类合计 0，事件量 2");
  // 整行没算出来的群两张表都留空，不比
  expect(build([row({ key: "r0", events: null, failedDays: 3 })], [])).not.toThrow();
  // 分类数据里冒出第一个 sheet 没有的群：两次取数之间新进来的，不能静默丢掉
  expect(
    build(
      [row({ key: "r1", events: 0 })],
      [{ roomid: "r9", counts: { urge_accept: 1 }, unlabeled: 0 }],
    ),
  ).toThrow("数据刚有更新（分类数据里多出 1 个群），请刷新页面后重新导出");
});
