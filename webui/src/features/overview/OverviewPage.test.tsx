/// <reference types="node" />
/** 概览指标、默认入口、下钻交互和 ECharts 实际渲染回归。 */

import { render, renderHook, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation } from "react-router-dom";
import { beforeAll, describe, expect, it, vi } from "vitest";
import * as echarts from "echarts";
import { buildMockDataset } from "@/api/mock/generator";
import { Providers } from "@/app/providers";
import { queryClient } from "@/app/queryClient";
import {
  agentRollup,
  buildTaxonomyIndex,
  categoryRows,
  decorate,
  roomRollup,
} from "@/domain/metrics";
import { mockAgentAggs, mockCategories, mockRoomAggs, mockSummary } from "@/api/mock/aggregate";
import { parentGroups } from "@/features/filters/useAnalytics";
import type { LoadedDataset } from "@/api/source";
import type { DecoratedEvent } from "@/domain/schemas";
import { parseFilters, useFilters, type Filters } from "@/features/filters/useFilters";
import { useAnalytics } from "@/features/filters/useAnalytics";
import { msgRollup, waitedSecFrom, type OverviewProps } from "./overviewMetrics";
import { OverviewPage } from "./OverviewPage";
import { OverviewDashboard } from "./OverviewDashboard";
import { Workbench } from "@/components/layout/Workbench";
import {
  buildCategoryPie,
  buildOverviewTrend,
  categorySlices,
  ResponseDistribution,
} from "./OverviewCharts";

vi.mock("@/components/charts/EChart", () => ({
  EChart: ({ ariaLabel }: { ariaLabel: string }) => <div role="img" aria-label={ariaLabel} />,
}));

/**
 * 页面的指标现在全部来自聚合接口，所以视图测试摆布的是**这批事件**，
 * 由 `mock/aggregate`（口径的前端对照实现）算成接口的形状 —— 不手写假数字。
 */
const stub = vi.hoisted(() => ({ events: [], groupDaily: [], tax: new Map() }) as never);
vi.mock("@/api/source", async (importOriginal) => {
  const { sourceStub } = await import("@/test/aggregateStub");
  return sourceStub(await importOriginal(), stub);
});
afterEach(() => queryClient.clear());
beforeAll(() => {
  // jsdom 里这两个是缺的（TS 的 DOM lib 却认为一定存在），AntD 的主题探测与
  // EChart 的 ResizeObserver 都要用，直接补上。
  window.matchMedia = (query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  });
  window.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
});

/**
 * 视图测试里的「一份数据」= 页面上下文（meta ＋ 群日）＋ **喂给聚合替身的那批事件**。
 * 事件本身不再进 `LoadedDataset`（页面不从那里拿），但测试要靠它摆布数字。
 */
type TestDataset = LoadedDataset & { events: DecoratedEvent[] };

function loadMock(): TestDataset {
  const raw = buildMockDataset();
  const taxIndex = buildTaxonomyIndex(raw.meta.taxonomy, raw.meta.taxonomy_version);
  return {
    source: "mock",
    fallbackReason: null,
    taxIndex,
    loadedAt: 0,
    meta: raw.meta,
    events: decorate(raw.events, taxIndex),
    groupDaily: raw.groupDaily,
  };
}

const dataset = loadMock();

/**
 * 断言的期望值和页面走的是**同一条口径实现**（`mock/aggregate`）——
 * 页面经过接口替身拿到它，测试直接调它。手写常量的话，口径错了测试照样绿。
 */
function aggsOf(ctx: OverviewProps, data: TestDataset = dataset) {
  const input = [data.events, data.groupDaily, data.taxIndex] as const;
  const groups = parentGroups(data).map((parent) => parent.types);
  return {
    summary: mockSummary(...input, ctx.analytics.q),
    rooms: mockRoomAggs(...input, ctx.analytics.q, groups),
    agents: mockAgentAggs(...input, ctx.analytics.q),
    level1: mockCategories(...input, ctx.analytics.q, groups),
  };
}

/** 把 hook 的产物取出来做纯函数断言，不必渲染整棵树。 */
function contextFor(search = "", data = dataset): OverviewProps {
  const filters: Filters = parseFilters(new URLSearchParams(search));
  const { result, unmount } = renderHook(
    () => ({ analytics: useTestAnalytics(data, filters), api: useFilters() }),
    {
      wrapper: ({ children }) => (
        <MemoryRouter initialEntries={[search ? `/overview?${search}` : "/overview"]}>
          {children}
        </MemoryRouter>
      ),
    },
  );
  // 只取 hook 的产物，不渲染页面，没有骨架屏要等。
  unmount();
  return result.current;
}

/**
 * 渲染前把这份数据接到聚合替身上 —— 页面上的每个数字仍然是**算出来的**，
 * 只是算它的输入由测试指定。必须在 `useAnalytics` 之前赋值：查询的
 * `queryFn` 在这之后才异步跑。
 */
function useTestAnalytics(...args: Parameters<typeof useAnalytics>) {
  const data = args[0] as TestDataset;
  Object.assign(stub, {
    events: data.events,
    groupDaily: data.groupDaily,
    tax: data.taxIndex,
  });
  return useAnalytics(...args);
}

/**
 * 骨架屏消失＝页面的聚合请求都落地了。取数已经是异步的，同步断言只会看到空壳。
 */
async function settle(view: { container: HTMLElement }) {
  await waitFor(() => expect(view.container.querySelector(".c2e-page")).toBeNull());
  return view;
}

describe("整体概览", () => {
  it("等待时长截至末日 24:00，且按工作时段计", () => {
    // 基准是数据窗口末日而不是 now()：末日 20:00 起算只剩收工前那一小时。
    // 若基准是 now()，这个数会随真实日期一路变大。
    expect(waitedSecFrom("2026-08-31", "2026-08-31 20:00:00")).toBe(3600);
    // 21:00 收工之后进来、当天没人接：工作时段里一秒都没等到，是 0 不是 1 秒。
    expect(waitedSecFrom("2026-08-31", "2026-08-31 23:59:59")).toBe(0);
    // 跨天积压：8/30 20:00 起，8/30 剩 1 小时 + 8/31 一整个工作日。
    expect(waitedSecFrom("2026-08-31", "2026-08-30 20:00:00")).toBe(3600 + 45000);
  });
  it("D 工作台展示消息总量", async () => {
    const props = contextFor();
    const view = render(
      <MemoryRouter initialEntries={["/overview"]}>
        <Providers>
          <OverviewDashboard {...props} />
        </Providers>
      </MemoryRouter>,
    );
    await settle(view);
    expect(screen.getAllByText(/消息/).length).toBeGreaterThan(0);
    // 消息总量必须真的出现在页面上，不是只有一个「消息」字样的标签
    const total = msgRollup(props.analytics).msgs.toLocaleString("zh-CN");
    expect(view.container.textContent).toContain(total);
    const labels = [
      ...view.container.querySelectorAll(".od-overview-metrics .od-metric-label"),
    ].map((label) => label.textContent);
    expect(labels.slice(0, 3)).toEqual(["活跃群", "消息总量", "事件量"]);
    view.unmount();
  });

  it("内容筛选不改变消息级指标分母", () => {
    const base = contextFor();
    const plain = msgRollup(base.analytics);
    expect(plain.msgs).toBeGreaterThan(0);

    // 分子随筛选变小、分母 msg_count 不变 —— 这个比值不成立，必须是 NULL 不是数字
    for (const q of ["status=unreplied", "overdue=1", "q=空调"]) {
      const ctx = contextFor(q);
      const r = msgRollup(ctx.analytics);
      expect(r.msgs, `${q} 不该改变消息量分母`).toBe(plain.msgs);
    }
  });

  it("概览移除辅助栏，保留客服入口、响应阈值与群日缺失标记", async () => {
    const props = contextFor();
    expect(props.analytics.cov.failed).toBeGreaterThan(0);
    const view = render(
      <MemoryRouter initialEntries={["/overview"]}>
        <Providers>
          <OverviewDashboard {...props} />
        </Providers>
      </MemoryRouter>,
    );
    await settle(view);
    const text = view.container.textContent;
    expect(text).not.toContain("在群聊洞察里定位");
    expect(text).not.toContain(`${props.analytics.cov.failed} 个群日抽取失败`);
    expect(
      screen.getByRole("link", { name: `活跃客服 ${aggsOf(props).summary.agents} 人` }),
    ).toHaveAttribute("href", expect.stringContaining("/agents"));
    expect(text).toContain("首响阈值 30 分钟");
    expect(
      view.container.querySelectorAll('.od-table [title*="NULL 不是 0"]').length,
    ).toBeGreaterThan(0);
    view.unmount();
  });

  /** 概览固定最近七天，URL 上残留的内容筛选如果还生效，
   *  读数的人会看到一个被悄悄收窄、又没有任何 UI 说得清的口径。 */
  it.each(["", "?variant=D", "?variant=E", "?variant=F", "?source=mock"])(
    "概览入口 %s 固定使用 D，清理旧参数并保留 drawer",
    async (search) => {
      const seen: string[] = [];
      function Probe() {
        seen.push(useLocation().search);
        return null;
      }
      function Harness() {
        const api = useFilters();
        const analytics = useTestAnalytics(dataset, api.filters);
        return (
          <>
            <Workbench>
              <OverviewPage analytics={analytics} api={api} />
            </Workbench>
            <Probe />
          </>
        );
      }
      const view = render(
        <MemoryRouter
          initialEntries={[
            `/overview${search}${search ? "&" : "?"}room=R-0001&from=2026-08-27&q=空调&drawer=999999`,
          ]}
        >
          <Providers>
            <Harness />
          </Providers>
        </MemoryRouter>,
      );
      await settle(view);
      await waitFor(() => {
        expect(seen.at(-1)).toBe(
          search.includes("source=mock") ? "?source=mock&drawer=999999" : "?drawer=999999",
        );
      });
      expect(view.container.querySelector('[data-skin="D"] .od-overview')).not.toBeNull();
      expect(screen.getByRole("option", { name: "无响应数" })).toBeInTheDocument();
      expect(screen.getByRole("columnheader", { name: "无响应" })).toBeInTheDocument();
      const link = screen.getByRole("link", { name: /^无响应事件：/ });
      expect(
        new URL(link.getAttribute("href")!, "http://localhost").searchParams.get("status"),
      ).toBe("unreplied");
      expect(view.container.querySelector(".pv-switch")).toBeNull();
      expect(view.container.textContent).not.toContain("未" + "回复");
      view.unmount();
    },
  );

  it("逐日消息量之和等于总量；样本里的抽取失败在逐日上看得见", () => {
    const ctx = contextFor();
    const r = msgRollup(ctx.analytics);
    expect(r.byDay.reduce((s, d) => s + d.msgs, 0)).toBe(r.msgs);
    expect(r.byDay.some((d) => d.failed > 0)).toBe(true);
  });
});

describe("概览的三块分布", () => {
  it("类型分布：各段计数之和等于事件总数，占比之和为 1", () => {
    const ctx = contextFor();
    const { analytics } = ctx;
    const { summary, level1 } = aggsOf(ctx);
    const cats = categoryRows(
      level1,
      "level1",
      analytics.taxIndex,
      analytics.parents,
      summary.events,
    );
    expect(cats.length).toBeGreaterThan(0);
    expect(
      cats.reduce((s, c) => s + c.count, 0),
      "有一级分类被静默吞掉了",
    ).toBe(summary.events);
    expect(cats.reduce((s, c) => s + c.share, 0)).toBeCloseTo(1, 6);
  });

  /** 排行条上「首响归属」那一段叠在「活跃量」里面：字段接反了内层就会超出外层。
   *  另一半是不变量 5 —— 对 agent 表求和只会**更小**，绝不会更大。 */
  it("客服：首响归属 ≤ 活跃量，总和不超过「事件数 − 无响应」", () => {
    const ctx = contextFor();
    const { analytics } = ctx;
    const { summary, agents } = aggsOf(ctx);
    const rows = agentRollup({
      aggs: agents,
      groupDaily: analytics.dataset.groupDaily,
      days: analytics.days,
      dayset: analytics.dayset,
      labelOf: analytics.agentLabel,
      query: analytics.query,
    });
    expect(rows.length).toBeGreaterThan(0);
    for (const a of rows)
      expect(a.owned, `${a.label}：首响归属不该超过活跃量，字段可能接反了`).toBeLessThanOrEqual(
        a.involved,
      );
    expect(rows.reduce((s, a) => s + a.owned, 0)).toBeLessThanOrEqual(
      summary.events - summary.unreplied,
    );
  });

  it("D 保留分布与归属口径，全部分类数量仍等于事件总数", async () => {
    const props = contextFor();
    const view = render(
      <MemoryRouter initialEntries={["/overview"]}>
        <Providers>
          <OverviewDashboard {...props} />
        </Providers>
      </MemoryRouter>,
    );
    await settle(view);
    const text = view.container.textContent;
    for (const t of ["事件类型分布", "活跃客服", "首响时长分布", "事件到达节奏", "群 × 日事件量"])
      expect(text, `${t} 没渲染出来`).toContain(t);
    // 三块排行瓦片（消息最多 / 无响应最多 / 超时率最高）并进了群表的汇总列。
    // 列没了 = 那三张榜的信息被静默丢掉，比瓦片消失更难发现。
    for (const h of ["消息总量", "无响应", "超时率", "首响 P50"])
      expect(text, `群表缺了「${h}」列`).toContain(h);
    expect(text).toContain("无响应不归属首响客服");
    const legend = [...view.container.querySelectorAll(".od-category-count")].map((el) =>
      Number(el.textContent),
    );
    const seen = aggsOf(props);
    expect(legend.length).toBe(Math.min(6, seen.level1.length));
    expect(
      legend.reduce((a, b) => a + b, 0),
      "折进「其他」的类被丢掉了",
    ).toBe(seen.summary.events);
    view.unmount();
  });
});

/**
 * 群表的行是**按消息量取前 N**，不是按事件量。这条极容易被「顺手改成按事件量排」
 * 掉进坑里：整段抽取失败的群 `events` 是 NULL，按事件量排会掉到榜底、被 slice 切掉 ——
 * 而那恰好是最该被看见的群（不变量 4：`Ok([])` 与 `Failed` 绝不混淆）。
 * jsdom 里看不出布局，但行的顺序是能查的。
 */
describe("D 的群表", () => {
  it("按消息量排序取前 10 —— 用不依赖抽取的那一列排，失败的群才不会从榜上消失", async () => {
    const props = contextFor();
    const { analytics } = props;
    const expected = roomRollup({
      aggs: aggsOf(props).rooms,
      groupDaily: analytics.dataset.groupDaily,
      rooms: analytics.dataset.meta.rooms,
      days: analytics.days,
      dayset: analytics.dayset,
      parents: analytics.parents,
      labelOf: analytics.roomLabel,
      query: analytics.query,
    })
      .sort((a, b) => (b.msgs ?? -1) - (a.msgs ?? -1))
      .slice(0, 10)
      .map((r) => r.label);

    const view = render(
      <MemoryRouter initialEntries={["/overview"]}>
        <Providers>
          <OverviewDashboard {...props} />
        </Providers>
      </MemoryRouter>,
    );
    await settle(view);
    const actual = [...view.container.querySelectorAll('.od-table th[scope="row"] a')].map((el) =>
      el.textContent.trim(),
    );
    expect(actual, "群表的行顺序变了：排序键是不是被改成 events 了？").toEqual(expected);
    const cells = view.container.querySelectorAll(".od-table tbody td.od-num");
    expect(cells.length).toBe(expected.length * 5);
    view.unmount();
  });
});

describe("D 概览指标边界与交互", () => {
  it("概览取最新七个自然日，缺日期不向前补旧数据，下钻保留同一窗口", async () => {
    const missing = new Set(["2026-08-25", "2026-08-27"]);
    const currentEvents = dataset.events.filter((e) => !missing.has(e.occurred_on));
    const oldEvent = { ...dataset.events[0]!, id: 99999, occurred_on: "2026-08-24" };
    const oldCell = { ...dataset.groupDaily[0]!, dt: "2026-08-24", msg_count: 99999 };
    const data: TestDataset = {
      ...dataset,
      meta: {
        ...dataset.meta,
        days: ["2026-08-24", ...dataset.meta.days.filter((d) => !missing.has(d))],
      },
      events: [oldEvent, ...currentEvents],
      groupDaily: [oldCell, ...dataset.groupDaily.filter((g) => !missing.has(g.dt))],
    };
    const props = contextFor("variant=D", data);
    const view = render(
      <MemoryRouter initialEntries={["/overview?variant=D"]}>
        <Providers>
          <OverviewPage {...props} />
        </Providers>
      </MemoryRouter>,
    );
    await settle(view);
    expect(view.container.textContent).toContain("最近 7 天");
    expect(view.container.textContent).toContain(`${2 * dataset.meta.rooms.length} 个群日无记录`);
    // ⚠️ **有缺格也要给日均，而且标成下界。** 此前缺格就把日均整个换掉，而缺格是常态
    // （`rotate_daily`：1000 群、每轮 400），于是这个数永远不显示 —— 而 `msg_count`
    // 是消息级列、不依赖抽取，手里这些格子的和是实打实的已知量，缺格只让它偏小。
    expect(view.container.textContent).toContain("日均 ≥ ");
    expect(view.container.textContent).not.toContain("日均暂缺");
    expect(view.container.textContent).not.toContain("完整性未知");
    const dates = [...view.container.querySelectorAll(".od-table thead .od-day")].map(
      (el) => el.textContent,
    );
    expect(dates).toEqual(["08-25", "08-26", "08-27", "08-28", "08-29", "08-30", "08-31"]);
    const link = screen.getByRole("link", {
      name: `事件量：${currentEvents.length.toLocaleString("zh-CN")}起，查看明细`,
    });
    const search = new URL(link.getAttribute("href")!, "http://localhost").search;
    expect(new URLSearchParams(search).get("from")).toBe("2026-08-25");
    expect(new URLSearchParams(search).get("to")).toBe("2026-08-31");
    view.unmount();
    // 窗口跟着链接走：下推给接口的日期区间就是链接上的那一段。
    expect(contextFor(search, data).analytics.q).toMatchObject({
      from: "2026-08-25",
      to: "2026-08-31",
    });
  });

  it("分类饼图最多六段且总数不变，其他分类可展开下钻", async () => {
    const props = contextFor();
    const { summary, level1 } = aggsOf(props);
    const cats = categoryRows(
      level1,
      "level1",
      props.analytics.taxIndex,
      props.analytics.parents,
      summary.events,
    );
    const slices = categorySlices(cats);
    expect(slices).toHaveLength(6);
    expect(slices.reduce((sum, s) => sum + s.count, 0)).toBe(summary.events);
    expect(slices.reduce((sum, s) => sum + s.share, 0)).toBeCloseTo(1);
    const chart = echarts.init(null, null, { renderer: "svg", ssr: true, width: 320, height: 196 });
    chart.setOption(buildCategoryPie(cats));
    expect(chart.renderToSVGString()).toContain("<path");
    chart.dispose();
    expect(categorySlices([])).toEqual([]);
    expect(categorySlices(cats.slice(0, 1))).toHaveLength(1);
    const view = render(
      <MemoryRouter>
        <Providers>
          <OverviewDashboard {...props} />
        </Providers>
      </MemoryRouter>,
    );
    await settle(view);
    await userEvent.click(screen.getByRole("button", { name: /其他 \d+ 类/ }));
    for (const row of cats.slice(5)) {
      const link = view.container.querySelector(
        `.od-pie-other a[href*="${encodeURIComponent(row.key)}"]`,
      );
      expect(link).not.toBeNull();
    }
    view.unmount();
  });

  it("首响分箱包含零秒和边界值，排除无响应及平台事件", async () => {
    // 分箱现在由数据库按 `RESPONSE_BIN_EDGES` 算好，组件只排标签 ——
    // 这里直接给那组计数，钉住的是「边界值落在哪个桶」的呈现，不是再分一次桶。
    const buckets = [2, 2, 1, 1, 1, 1, 1, 1];
    const view = render(<ResponseDistribution buckets={buckets} replied={10} slaSec={1800} />);
    await settle(view);
    const counts = [...view.container.querySelectorAll(".od-bin-count")].map((el) =>
      Number(el.textContent),
    );
    expect(counts).toEqual([2, 2, 1, 1, 1, 1, 1, 1]);
    expect(counts.reduce((sum, n) => sum + n, 0)).toBe(10);
    view.unmount();
  });

  it("全部抽取失败仍展示消息量，事件指标和群表不伪装成零", async () => {
    const data: TestDataset = {
      ...dataset,
      events: [],
      groupDaily: dataset.groupDaily.map((g) => ({ ...g, extraction_status: "failed" })),
    };
    const props = contextFor("", data);
    const view = render(
      <MemoryRouter>
        <Providers>
          <OverviewDashboard {...props} />
        </Providers>
      </MemoryRouter>,
    );
    await settle(view);
    expect(view.container.textContent).toContain(formatIntForTest(msgRollup(props.analytics).msgs));
    expect(screen.queryByRole("link", { name: /^事件量：/ })).not.toBeInTheDocument();
    expect(
      within(screen.getByRole("region", { name: "核心指标" }))
        .getByText("事件量", { exact: true })
        .closest(".od-metric"),
    ).toHaveTextContent("—起");
    expect(view.container.querySelector(".od-table tbody tr td:nth-child(3)")?.textContent).toBe(
      "—",
    );
    expect(view.container.textContent).not.toContain("商家事件均已回复");
    view.unmount();
  });

  it("空窗口没有虚构峰值或成功响应，群日消息记录继续显示", async () => {
    const props = contextFor("", {
      ...dataset,
      events: [],
      groupDaily: dataset.groupDaily.map((g) => ({ ...g, extraction_status: "ok" })),
    });
    const view = render(
      <MemoryRouter>
        <Providers>
          <OverviewDashboard {...props} />
        </Providers>
      </MemoryRouter>,
    );
    await settle(view);
    expect(view.container.textContent).toContain("当前窗口没有商家发起事件");
    await userEvent.click(screen.getByRole("button", { name: "事件到达节奏" }));
    expect(screen.getByRole("button", { name: "事件到达节奏" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    view.unmount();
  });

  it("群聊按无响应排序，选择事件可打开并关闭现有详情抽屉", async () => {
    function Harness() {
      const api = useFilters();
      return <OverviewDashboard analytics={useTestAnalytics(dataset, api.filters)} api={api} />;
    }
    const view = render(
      <MemoryRouter initialEntries={["/overview?variant=D"]}>
        <Providers>
          <Harness />
        </Providers>
      </MemoryRouter>,
    );
    await settle(view);
    await userEvent.selectOptions(screen.getByRole("combobox", { name: "群聊排序" }), "unreplied");
    const values = [...view.container.querySelectorAll(".od-table tbody tr")].map((row) =>
      Number(row.querySelectorAll("td.od-num")[2]?.textContent),
    );
    expect(values).toEqual([...values].sort((a, b) => b - a));
    const queueLink = view.container.querySelector<HTMLAnchorElement>(".od-queue-row")!;
    await userEvent.click(queueLink);
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveAccessibleName(/事件 #/);
    await userEvent.click(within(dialog).getByRole("button", { name: "关闭" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    view.unmount();
  });

  it("D 的独立标尺与小时图可真实渲染，全部失败日期在事件线上断开", () => {
    const props = contextFor();
    const msgs = msgRollup(props.analytics).byDay;
    for (const hourly of [false, true]) {
      const chart = echarts.init(null, null, {
        renderer: "svg",
        ssr: true,
        width: 680,
        height: 338,
      });
      chart.setOption(buildOverviewTrend(props.analytics, aggsOf(props).summary, msgs, hourly));
      const svg = chart.renderToSVGString();
      expect(svg).toContain("<path");
      expect(svg).toContain("事件 / 起");
      if (!hourly) expect(svg).toContain("消息总量 / 条");
      chart.dispose();
    }
    const option = buildOverviewTrend(
      props.analytics,
      aggsOf(props).summary,
      msgs.map((m) => ({ ...m, failed: m.cells })),
      false,
    );
    const series = option.series as { name: string; data: unknown[] }[];
    expect(series.map((item) => item.name)).toEqual(["消息总量", "事件量", "无响应"]);
    expect(series.find((item) => item.name === "事件量")?.data).toEqual(msgs.map(() => null));
  });
});

const formatIntForTest = (n: number) => n.toLocaleString("zh-CN");
