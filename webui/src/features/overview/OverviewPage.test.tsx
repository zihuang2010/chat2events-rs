/// <reference types="node" />
/** 概览指标、默认入口、下钻交互和 ECharts 实际渲染回归。 */

import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation } from "react-router-dom";
import { beforeAll, describe, expect, it, vi } from "vitest";
import * as echarts from "echarts";
import { buildMockDataset } from "@/api/mock/generator";
import { Providers } from "@/app/providers";
import {
  agentRollup,
  buildTaxonomyIndex,
  categoryRollup,
  decorate,
  roomRollup,
} from "@/domain/metrics";
import type { LoadedDataset } from "@/api/source";
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

function loadMock(): LoadedDataset {
  const raw = buildMockDataset();
  const taxIndex = buildTaxonomyIndex(raw.meta.taxonomy);
  return {
    source: "mock",
    fallbackReason: null,
    taxIndex,
    loadedAt: 0,
    meta: raw.meta,
    events: decorate(raw.events, taxIndex),
    groupDaily: raw.groupDaily,
    agentDaily: raw.agentDaily,
    failures: raw.failures,
  };
}

const dataset = loadMock();

/** 用一个探针组件把 hook 的产物取出来，纯函数断言不必渲染整棵树 */
function contextFor(search = "", data = dataset): OverviewProps {
  const filters: Filters = parseFilters(new URLSearchParams(search));
  // 用对象属性而不是局部变量：赋值发生在 render 的回调里，TS narrow 不到那一步
  const box: { value: OverviewProps | null } = { value: null };
  function Probe() {
    box.value = { analytics: useAnalytics(data, filters), api: useFilters() };
    return null;
  }
  const view = render(
    <MemoryRouter initialEntries={[search ? `/overview?${search}` : "/overview"]}>
      <Probe />
    </MemoryRouter>,
  );
  view.unmount();
  if (box.value === null) throw new Error("探针没跑起来");
  return box.value;
}

describe("整体概览", () => {
  it("等待时长准确截至末日 24:00", () => {
    expect(waitedSecFrom("2026-08-31", "2026-08-31 23:59:59")).toBe(1);
  });
  it("D 工作台展示消息总量", () => {
    const props = contextFor();
    const view = render(
      <MemoryRouter initialEntries={["/overview"]}>
        <Providers>
          <OverviewDashboard {...props} />
        </Providers>
      </MemoryRouter>,
    );
    expect(screen.getAllByText(/消息/).length).toBeGreaterThan(0);
    // 消息总量必须真的出现在页面上，不是只有一个「消息」字样的标签
    const total = msgRollup(props.analytics, props.api).msgs.toLocaleString("zh-CN");
    expect(view.container.textContent).toContain(total);
    view.unmount();
  });

  it("内容筛选不改变消息级指标分母", () => {
    const base = contextFor();
    const plain = msgRollup(base.analytics, base.api);
    expect(plain.msgs).toBeGreaterThan(0);

    // 分子随筛选变小、分母 msg_count 不变 —— 这个比值不成立，必须是 NULL 不是数字
    for (const q of ["status=unreplied", "overdue=1", "q=空调"]) {
      const ctx = contextFor(q);
      const r = msgRollup(ctx.analytics, ctx.api);
      expect(r.msgs, `${q} 不该改变消息量分母`).toBe(plain.msgs);
    }
  });

  it("D · 紧凑完整性提示与群日缺失标记保留抽取失败信号", () => {
    const props = contextFor();
    expect(props.analytics.cov.failed).toBeGreaterThan(0);
    const view = render(
      <MemoryRouter initialEntries={["/overview"]}>
        <Providers>
          <OverviewDashboard {...props} />
        </Providers>
      </MemoryRouter>,
    );
    const text = view.container.textContent;
    expect(text).not.toContain("在群聊洞察里定位");
    expect(text).toContain(`${props.analytics.cov.failed} 个群日抽取失败`);
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
        const analytics = useAnalytics(dataset, api.filters);
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
    const r = msgRollup(ctx.analytics, ctx.api);
    expect(r.byDay.reduce((s, d) => s + d.msgs, 0)).toBe(r.msgs);
    expect(r.byDay.some((d) => d.failed > 0)).toBe(true);
  });
});

describe("概览的三块分布", () => {
  it("类型分布：各段计数之和等于事件总数，占比之和为 1", () => {
    const { analytics } = contextFor();
    const cats = categoryRollup(analytics.events, "level1", analytics.taxIndex);
    expect(cats.length).toBeGreaterThan(0);
    expect(
      cats.reduce((s, c) => s + c.count, 0),
      "有一级分类被静默吞掉了",
    ).toBe(analytics.agg.events);
    expect(cats.reduce((s, c) => s + c.share, 0)).toBeCloseTo(1, 6);
  });

  /** 排行条上「首响归属」那一段叠在「活跃量」里面：字段接反了内层就会超出外层。
   *  另一半是不变量 5 —— 对 agent 表求和只会**更小**，绝不会更大。 */
  it("客服：首响归属 ≤ 活跃量，总和不超过「事件数 − 无响应」", () => {
    const { analytics } = contextFor();
    const rows = agentRollup({
      events: analytics.events,
      groupDaily: analytics.dataset.groupDaily,
      agents: analytics.dataset.meta.agents,
      days: analytics.days,
      dayset: analytics.dayset,
      slaSec: analytics.slaSec,
      labelOf: analytics.agentLabel,
      query: analytics.query,
    });
    expect(rows.length).toBeGreaterThan(0);
    for (const a of rows)
      expect(a.owned, `${a.label}：首响归属不该超过活跃量，字段可能接反了`).toBeLessThanOrEqual(
        a.involved,
      );
    expect(rows.reduce((s, a) => s + a.owned, 0)).toBeLessThanOrEqual(
      analytics.agg.events - analytics.agg.unreplied,
    );
  });

  it("D 保留分布与归属口径，全部分类数量仍等于事件总数", () => {
    const props = contextFor();
    const view = render(
      <MemoryRouter initialEntries={["/overview"]}>
        <Providers>
          <OverviewDashboard {...props} />
        </Providers>
      </MemoryRouter>,
    );
    const text = view.container.textContent;
    for (const t of ["事件类型分布", "活跃客服", "首响时长分布", "事件到达节奏", "群 × 日事件量"])
      expect(text, `${t} 没渲染出来`).toContain(t);
    // 三块排行瓦片（消息最多 / 无响应最多 / 超时率最高）并进了群表的汇总列。
    // 列没了 = 那三张榜的信息被静默丢掉，比瓦片消失更难发现。
    for (const h of ["消息量", "无响应", "超时率", "首响 P50"])
      expect(text, `群表缺了「${h}」列`).toContain(h);
    expect(text).toContain("无响应不归属首响客服");
    const legend = [...view.container.querySelectorAll(".od-category-count")].map((el) =>
      Number(el.textContent),
    );
    expect(legend.length).toBe(
      Math.min(
        6,
        categoryRollup(props.analytics.events, "level1", props.analytics.taxIndex).length,
      ),
    );
    expect(
      legend.reduce((a, b) => a + b, 0),
      "折进「其他」的类被丢掉了",
    ).toBe(props.analytics.agg.events);
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
  it("按消息量排序取前 10 —— 用不依赖抽取的那一列排，失败的群才不会从榜上消失", () => {
    const props = contextFor();
    const { analytics } = props;
    const expected = roomRollup({
      events: analytics.events,
      groupDaily: analytics.dataset.groupDaily,
      rooms: analytics.dataset.meta.rooms,
      days: analytics.days,
      dayset: analytics.dayset,
      slaSec: analytics.slaSec,
      lastDay: analytics.lastDay,
      labelOf: analytics.roomLabel,
      query: analytics.query,
    })
      .sort((a, b) => b.msgs - a.msgs)
      .slice(0, 10)
      .map((r) => r.label);

    const view = render(
      <MemoryRouter initialEntries={["/overview"]}>
        <Providers>
          <OverviewDashboard {...props} />
        </Providers>
      </MemoryRouter>,
    );
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
  it("概览取最新七个自然日，缺日期不向前补旧数据，下钻保留同一窗口", () => {
    const missing = new Set(["2026-08-25", "2026-08-27"]);
    const currentEvents = dataset.events.filter((e) => !missing.has(e.occurred_on));
    const oldEvent = { ...dataset.events[0]!, id: 99999, occurred_on: "2026-08-24" };
    const oldCell = { ...dataset.groupDaily[0]!, dt: "2026-08-24", msg_count: 99999 };
    const data: LoadedDataset = {
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
    expect(view.container.textContent).toContain("最近 7 天");
    expect(view.container.textContent).toContain("2 天无记录");
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
    expect(contextFor(search, data).analytics.events).toEqual(currentEvents);
  });

  it("分类饼图最多六段且总数不变，其他分类可展开下钻", async () => {
    const props = contextFor();
    const cats = categoryRollup(props.analytics.events, "level1", props.analytics.taxIndex);
    const slices = categorySlices(cats);
    expect(slices).toHaveLength(6);
    expect(slices.reduce((sum, s) => sum + s.count, 0)).toBe(props.analytics.agg.events);
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
    await userEvent.click(screen.getByRole("button", { name: /其他 \d+ 类/ }));
    for (const row of cats.slice(5)) {
      const link = view.container.querySelector(
        `.od-pie-other a[href*="${encodeURIComponent(row.key)}"]`,
      );
      expect(link).not.toBeNull();
    }
    view.unmount();
  });

  it("首响分箱包含零秒和边界值，排除无响应及平台事件", () => {
    const seed = dataset.events.find((e) => e.asker_role === "EXTERNAL")!;
    const secs = [0, 60, 61, 300, 900, 1800, 3600, 7200, 14400, 14401, null];
    const events = secs.map((sec, i) => ({ ...seed, id: i, firstReplySec: sec }));
    events.push({ ...seed, id: 99, asker_role: "INTERNAL", firstReplySec: 0 });
    const view = render(<ResponseDistribution events={events} slaSec={1800} />);
    const counts = [...view.container.querySelectorAll(".od-bin-count")].map((el) =>
      Number(el.textContent),
    );
    expect(counts).toEqual([2, 2, 1, 1, 1, 1, 1, 1]);
    expect(counts.reduce((sum, n) => sum + n, 0)).toBe(10);
    view.unmount();
  });

  it("全部抽取失败仍展示消息量，事件指标和群表不伪装成零", () => {
    const data: LoadedDataset = {
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
    expect(view.container.textContent).toContain(
      formatIntForTest(msgRollup(props.analytics, props.api).msgs),
    );
    expect(screen.getByRole("link", { name: "事件量：—起，查看明细" })).toBeInTheDocument();
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
      return <OverviewDashboard analytics={useAnalytics(dataset, api.filters)} api={api} />;
    }
    const view = render(
      <MemoryRouter initialEntries={["/overview?variant=D"]}>
        <Providers>
          <Harness />
        </Providers>
      </MemoryRouter>,
    );
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
    const msgs = msgRollup(props.analytics, props.api).byDay;
    for (const hourly of [false, true]) {
      const chart = echarts.init(null, null, {
        renderer: "svg",
        ssr: true,
        width: 680,
        height: 338,
      });
      chart.setOption(buildOverviewTrend(props.analytics, msgs, hourly));
      const svg = chart.renderToSVGString();
      expect(svg).toContain("<path");
      expect(svg).toContain("事件 / 起");
      if (!hourly) expect(svg).toContain("消息 / 条");
      chart.dispose();
    }
    const option = buildOverviewTrend(
      props.analytics,
      msgs.map((m) => ({ ...m, failed: m.cells })),
      false,
    );
    const series = option.series as { data: unknown[] }[];
    expect(series[0]?.data).toEqual(msgs.map(() => null));
  });
});

const formatIntForTest = (n: number) => n.toLocaleString("zh-CN");
