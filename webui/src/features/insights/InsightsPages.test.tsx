import { act, cleanup, render, renderHook, screen, within, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation } from "react-router-dom";
import { afterEach, beforeAll, beforeEach, expect, it, vi } from "vitest";
import { buildMockDataset } from "@/test/mock/generator";
import { buildTaxonomyIndex, decorate } from "@/domain/metrics";
import type { LoadedDataset } from "@/api/source";
import { Providers } from "@/app/providers";
import { queryClient } from "@/app/queryClient";
import { Workbench } from "@/components/layout/Workbench";
import { parseFilters, useFilters } from "@/features/filters/useFilters";
import { useAnalytics } from "@/features/filters/useAnalytics";
import type { EChartProps } from "@/components/charts/EChart";
import { EventsPage } from "@/features/events/EventsPage";
import { AgentsPage } from "@/features/agents/AgentsPage";
import { RoomsPage } from "@/features/rooms/RoomsPage";
import { DetailPage } from "@/features/detail/DetailPage";
import type { DecoratedEvent, MessageRow } from "@/domain/schemas";
import type * as Queries from "@/api/queries";
import { formatInt } from "@/lib/format";

const messages = vi.hoisted(() => {
  const data: MessageRow[] = [];
  return { mode: "empty", data, retry: vi.fn() };
});
vi.mock("@/api/queries", async (importOriginal) => ({
  ...(await importOriginal<typeof Queries>()),
  useEventMessages: () => ({
    data: messages.data,
    isPending: messages.mode === "loading",
    isError: messages.mode === "error",
    error: new Error("原文服务暂不可用"),
    isFetching: false,
    refetch: messages.retry,
  }),
}));
vi.mock("@/components/charts/EChart", () => ({
  EChart: ({ ariaLabel, option }: EChartProps) => (
    <div role="img" aria-label={ariaLabel} data-series={JSON.stringify(option.series)} />
  ),
}));
/**
 * 页面的指标现在全部来自聚合接口，所以视图测试摆布的是**这批事件**，
 * 由 `mock/aggregate`（口径的前端对照实现）算成接口的形状 —— 不手写假数字。
 */
const stub = vi.hoisted(() => ({ events: [], groupDaily: [], tax: new Map() }) as never);
/** 明细页实际请求过的页码 —— 翻页护栏只有在这里看得见（模拟数据源不会像真接口那样 400）。 */
const pageCalls = vi.hoisted(() => [] as number[]);
vi.mock("@/api/source", async (importOriginal) => {
  const { sourceStub } = await import("@/test/aggregateStub");
  const stubbed = sourceStub(await importOriginal(), stub);
  return {
    ...stubbed,
    loadEventsPage: (...args: Parameters<typeof stubbed.loadEventsPage>) => {
      pageCalls.push(args[1]);
      return stubbed.loadEventsPage(...args);
    },
  };
});
afterEach(cleanup);
// 缓存是模块级单例，用例之间不清就会读到上一个用例的数字。
afterEach(() => queryClient.clear());
afterEach(() => vi.unstubAllGlobals());
beforeEach(() => {
  pageCalls.length = 0;
  messages.mode = "empty";
  messages.data = [];
  messages.retry.mockClear();
});
beforeAll(() => {
  window.matchMedia = (query: string) => ({
    matches: true,
    media: query,
    onchange: null,
    addEventListener() {},
    removeEventListener() {},
    addListener() {},
    removeListener() {},
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

const raw = buildMockDataset();
const taxIndex = buildTaxonomyIndex(raw.meta.taxonomy, raw.meta.taxonomy_version);
const dataset: TestDataset = {
  meta: raw.meta,
  groupDaily: raw.groupDaily,
  events: decorate(raw.events, taxIndex),
  taxIndex,
  source: "api",
  loadedAt: 0,
};
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

const components = { events: EventsPage, agents: AgentsPage, detail: DetailPage, rooms: RoomsPage };

function Harness({ page, data }: { page: keyof typeof components; data: TestDataset }) {
  const api = useFilters();
  const analytics = useTestAnalytics(data, api.filters);
  const location = useLocation();
  const Page = components[page];
  return (
    <>
      <Page analytics={analytics} api={api} />
      <output data-testid="url">
        {location.pathname}
        {location.search}
      </output>
    </>
  );
}
/**
 * 渲染并**等聚合请求落地**。页面先出骨架屏 —— 取数已经不是同步的了，
 * 同步断言只会看到空壳。替身返回的是已 resolve 的 Promise，冲一轮微任务就够。
 */
async function mount(page: keyof typeof components, search = "", data = dataset) {
  // ⚠️ **同一个用例里挂第二次也要清缓存**：聚合查询的 key 只含筛选条件，
  // 不含「这次喂了什么数据」—— 不清的话第二次会原样读到第一次的数字。
  queryClient.clear();
  const view = render(
    <MemoryRouter initialEntries={[`/${page}${search}`]}>
      <Providers>
        <Workbench>
          <Harness page={page} data={data} />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  // 骨架屏消失＝聚合请求都落地了。固定冲几轮微任务不够可靠：
  // 一个页面上有四五个查询，落地和重渲染不一定挤在同一轮里。
  await waitFor(() => expect(view.container.querySelector(".c2e-page")).toBeNull());
  // 抽屉里的事件是页面渲染出来之后才按 id 去取的，再冲一轮。
  await act(async () => {});
  return view;
}

it("shows_twenty_agents_per_page_by_default", async () => {
  const user = userEvent.setup();
  const source = dataset.events.find((event) => event.first_responder !== null)!;
  const agents = Array.from({ length: 25 }, (_, index) => ({
    agent: `agent-${index}`,
    alias: `测试客服 ${index + 1}`,
  }));
  const data: TestDataset = {
    ...dataset,
    meta: { ...dataset.meta, agents },
    events: agents.map(({ agent }, index) => ({
      ...source,
      id: index + 1,
      agents: [agent],
      first_responder: agent,
    })),
  };
  const view = await mount("agents", "", data);
  expect(view.container.querySelectorAll(".ag-table .ia-table-link")).toHaveLength(20);
  expect(view.container.querySelector(".ant-pagination-total-text")).toHaveTextContent(
    "1 - 20 / 共 25 人",
  );
  await user.click(view.container.querySelector<HTMLElement>(".ant-pagination-item-2")!);
  expect(view.container.querySelectorAll(".ag-table .ia-table-link")).toHaveLength(5);
});

it.each(["rooms", "agents"] as const)(
  "paginates_rows_and_preserves_drawer_page_%s",
  async (page) => {
    const user = userEvent.setup();
    const view = await mount(page, "?size=5");
    const selector = page === "rooms" ? ".ra-room-link" : ".ag-table .ia-table-link";
    const firstPage = [...view.container.querySelectorAll(selector)].map((row) => row.textContent);
    expect(firstPage).toHaveLength(5);
    const total = view.container.querySelector(".ant-pagination-total-text")!.textContent;
    await user.click(view.container.querySelector<HTMLElement>(".ant-pagination-item-2")!);
    expect(screen.getByTestId("url")).toHaveTextContent("page=2");
    const secondPage = [...view.container.querySelectorAll(selector)].map((row) => row.textContent);
    expect(secondPage.length).toBeGreaterThan(0);
    expect(secondPage.every((label) => !firstPage.includes(label))).toBe(true);
    expect(view.container.querySelector(".ant-pagination-total-text")).toHaveTextContent(
      total.split(" / ")[1]!,
    );
    await user.click(view.container.querySelector<HTMLButtonElement>(selector)!);
    expect(await screen.findByRole("dialog")).toBeInTheDocument();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(screen.getByTestId("url")).toHaveTextContent("page=2");
    expect([...view.container.querySelectorAll(selector)].map((row) => row.textContent)).toEqual(
      secondPage,
    );
    await user.click(screen.getByRole("columnheader", { name: page === "rooms" ? "群" : "客服" }));
    expect(screen.getByTestId("url")).not.toHaveTextContent("page=2");
    expect(view.container.querySelectorAll(selector)).toHaveLength(5);
  },
);

it.each(["rooms", "agents"] as const)("clamps_out_of_range_page_%s", async (page) => {
  const view = await mount(page, "?page=999&size=5");
  const selector = page === "rooms" ? ".ra-room-link" : ".ag-table .ia-table-link";
  expect(view.container.querySelectorAll(selector).length).toBeGreaterThan(0);
  expect(view.container.querySelector(".ant-pagination-item-active")).not.toHaveTextContent("999");
});

it.each(["events", "agents", "detail"] as const)(
  "orders_primary_metrics_consistently_%s",
  async (page) => {
    const view = await mount(page);
    const labels = [
      ...view.container.querySelectorAll(".ia-workbench > .ia-metrics .od-metric-label"),
    ].map((label) => label.textContent);
    const expected =
      page === "events"
        ? ["活跃群", "消息总量", "事件量"]
        : page === "detail"
          ? ["活跃群", "来源消息数", "事件量"]
          : ["事件量", "首响中位时长", "事件超时率", "按时回复", "超时回复", "无响应"];
    expect(labels.slice(0, expected.length)).toEqual(expected);
  },
);

it("真实数据深链接按 ID 加载当前窗口之外的事件", async () => {
  const event = dataset.events[0]!;
  const fetch = vi.fn((url: URL) =>
    Promise.resolve(
      url.pathname === `/api/event/${event.id}`
        ? Response.json(event)
        : new Response(null, { status: 404 }),
    ),
  );
  vi.stubGlobal("fetch", fetch);
  await mount("detail", `?drawer=${event.id}`, { ...dataset, source: "api", events: [] });
  expect(await screen.findByText(event.summary)).toBeInTheDocument();
  expect(screen.getByText("这条事件不在当前筛选结果里")).toBeInTheDocument();
  expect(fetch.mock.calls.some(([url]) => url.pathname === `/api/event/${event.id}`)).toBe(true);
  expect(screen.queryByText(`找不到事件 #${event.id}`)).not.toBeInTheDocument();
});

it("bounds_calendar_expansion_to_the_loaded_date_range", () => {
  const filters = parseFilters(new URLSearchParams("from=0100-01-01&to=9999-12-31"));
  const { result } = renderHook(() => useTestAnalytics(dataset, filters));
  expect(result.current.days).toEqual(dataset.meta.days);
  // Analytics 不再持有事件明细，它现在产出的是**下推给聚合接口的那组参数**。
  expect(result.current.q).toMatchObject({
    from: dataset.meta.days[0],
    to: dataset.meta.days.at(-1),
  });
});

it.each(["from=0100-01-01&to=0200-01-01", "from=9000-01-01&to=9999-12-31"])(
  "keeps_disjoint_date_windows_empty_%s",
  (search) => {
    const filters = parseFilters(new URLSearchParams(search));
    const { result } = renderHook(() => useTestAnalytics(dataset, filters));
    expect(result.current.days).toEqual([]);
    expect(result.current.cov.cells).toBe(0);
    // 窗口与已加载范围不相交：`from > to` 的区间发给后端必然是空结果。
    expect(result.current.q.from! > result.current.q.to!).toBe(false);
  },
);

it("preserves_explicit_overview_days_outside_the_loaded_range", () => {
  const filters = parseFilters(new URLSearchParams("from=9000-01-01&to=9999-12-31"));
  const windowDays = ["2026-08-24", ...dataset.meta.days];
  const { result } = renderHook(() => useTestAnalytics(dataset, filters, windowDays));
  expect(result.current.days).toEqual(windowDays);
  expect(result.current.q).toMatchObject({ from: windowDays[0], to: windowDays.at(-1) });
});

it("shows_unknown_agent_coverage_when_another_room_has_no_record", async () => {
  const event = dataset.events.find((row) => row.agents.length > 0)!;
  const day = event.occurred_on;
  const data: TestDataset = {
    ...dataset,
    meta: { ...dataset.meta, days: [day] },
    events: [event],
    groupDaily: dataset.groupDaily.filter((row) => row.roomid === event.roomid && row.dt === day),
  };
  const view = await mount("agents", "", data);
  expect(view.container.textContent).toContain("完整性未知");
});

it("preserves_unrecorded_calendar_days_in_agent_coverage", async () => {
  const day = dataset.meta.days[0]!;
  const last = dataset.meta.days[2]!;
  const event = dataset.events.find((row) => row.agents.length > 0 && row.occurred_on === day)!;
  const data: TestDataset = {
    ...dataset,
    meta: { ...dataset.meta, days: [day, last], rooms: [{ roomid: event.roomid, alias: null }] },
    events: [event],
    groupDaily: dataset.groupDaily.filter(
      (row) => row.roomid === event.roomid && [day, last].includes(row.dt),
    ),
  };
  const view = await mount("agents", "", data);
  expect(view.container.textContent).toContain("完整性未知");
});

/**
 * 顶部完整性提示的条件是整个 coverage，**不只是打标** —— 一个只有抽取失败、
 * 打标全好的窗口曾经什么都不显示，而抽取失败正是最该出现在顶部的那种不完整。
 */
it("surfaces_extraction_failure_in_the_header_notice", async () => {
  const event = dataset.events[0]!;
  const day = event.occurred_on;
  const row = dataset.groupDaily.find((r) => r.roomid === event.roomid && r.dt === day)!;
  const data: TestDataset = {
    ...dataset,
    meta: { ...dataset.meta, days: [day], rooms: [{ roomid: event.roomid, alias: null }] },
    events: [event],
    groupDaily: [{ ...row, extraction_status: "failed", classification_status: "ok" }],
  };
  const view = await mount("events", "", data);
  expect(view.container.querySelector('[role="status"]')?.textContent).toContain(
    "1 个群日抽取失败",
  );
});

it("labels_v0_as_missing_taxonomy_in_the_category_view", async () => {
  const events = dataset.events.slice(0, 1).map((event) => ({
    ...event,
    taxonomy_version: "v0",
    event_type: "__untyped__",
  }));
  const taxIndex = buildTaxonomyIndex([], "v0");
  const view = await mount("events", "", {
    ...dataset,
    meta: { ...dataset.meta, taxonomy_version: "v0", taxonomy: [] },
    taxIndex,
    events: decorate(events, taxIndex),
  });
  expect(view.container.textContent).toContain("未建词表事件");
  expect(view.container.textContent).not.toContain("归不上去");
});

it.each(["events", "agents", "detail"] as const)(
  "keeps_failed_event_metrics_unknown_%s",
  async (page) => {
    const data: TestDataset = {
      ...dataset,
      events: [],
      groupDaily: dataset.groupDaily.map((row) => ({ ...row, extraction_status: "failed" })),
    };
    const view = await mount(page, "", data);
    const values = Array.from(view.container.querySelectorAll(".ia-metrics .od-metric-value"));
    expect(values).toHaveLength(6);
    if (page === "events") {
      const messageMetric = screen.getByText("消息总量", { exact: true }).parentElement!;
      expect(messageMetric.querySelector(".od-metric-value")).toHaveTextContent(
        formatInt(data.groupDaily.reduce((sum, row) => sum + row.msg_count, 0)) + "条",
      );
      expect(
        values
          .filter((value) => !messageMetric.contains(value))
          .every((value) => value.textContent.startsWith("—")),
      ).toBe(true);
    } else {
      expect(values.every((value) => value.textContent.startsWith("—"))).toBe(true);
    }
    expect(view.container.querySelector(".ag-response")).toBeNull();
  },
);

it.each(["", "&l1=missing&l2=missing&agent=missing&status=unreplied&overdue=1&q=missing"])(
  "limits_message_total_to_date_and_room_only_%s",
  async (extra) => {
    const cell = dataset.groupDaily[0]!;
    const otherDay = dataset.meta.days.find((day) => day !== cell.dt)!;
    const otherRoom = dataset.meta.rooms.find((room) => room.roomid !== cell.roomid)!.roomid;
    await mount("events", `?from=${cell.dt}&to=${cell.dt}&room=${cell.roomid}${extra}`, {
      ...dataset,
      events: [],
      groupDaily: [
        { ...cell, msg_count: 17, extraction_status: "failed" },
        { ...cell, dt: otherDay, msg_count: 100 },
        { ...cell, roomid: otherRoom, msg_count: 200 },
      ],
    });
    const metric = screen.getByText("消息总量", { exact: true }).parentElement!;
    expect(metric.querySelector(".od-metric-value")).toHaveTextContent("17条");
    expect(metric).toHaveTextContent("仅按日期、群统计");
  },
);

it.each(["events", "detail"] as const)("keeps_missing_message_data_unknown_%s", async (page) => {
  await mount(page, "", { ...dataset, events: [], groupDaily: [] });
  const label = page === "events" ? "消息总量" : "来源消息数";
  const metric = screen.getByText(label, { exact: true }).parentElement!;
  expect(metric.querySelector(".od-metric-value")).toHaveTextContent("—条");
});

it("marks_partial_message_total_as_known_only", async () => {
  const cell = dataset.groupDaily[0]!;
  await mount("events", "", {
    ...dataset,
    events: [],
    groupDaily: [{ ...cell, msg_count: 17 }],
  });
  const metric = screen.getByText("消息总量", { exact: true }).parentElement!;
  expect(metric.querySelector(".od-metric-value")).toHaveTextContent("17条");
  expect(metric).toHaveTextContent("仅已知量");
});

it.each(["events", "detail"] as const)("shows_known_zero_messages_%s", async (page) => {
  await mount(page, "", {
    ...dataset,
    events: [],
    groupDaily: dataset.groupDaily.map((row) => ({ ...row, msg_count: 0 })),
  });
  const label = page === "events" ? "消息总量" : "来源消息数";
  const metric = screen.getByText(label, { exact: true }).parentElement!;
  expect(metric.querySelector(".od-metric-value")).toHaveTextContent(/^0条$/);
});

it.each(["all", "page", "room", "empty"] as const)(
  "deduplicates_source_messages_across_matching_events_%s",
  async (scope) => {
    const event = dataset.events[0]!;
    const other = dataset.events.find((row) => row.roomid !== event.roomid)!;
    const events = Array.from({ length: 21 }, (_, index) => ({
      ...event,
      id: 10000 + index,
      source_msg_ids: ["shared", `message-${index}`, "shared"],
    }));
    events.push({ ...other, id: 10021, source_msg_ids: ["shared"] });
    const search = {
      all: "",
      page: "?page=2&size=20",
      room: `?room=${event.roomid}`,
      empty: "?q=no-matching-source-event",
    }[scope];
    await mount("detail", search, { ...dataset, events });
    const metric = screen.getByText("来源消息数", { exact: true }).parentElement!;
    const expected = scope === "empty" ? 0 : scope === "room" ? 22 : 23;
    expect(metric.querySelector(".od-metric-value")).toHaveTextContent(`${expected}条`);
  },
);

it("二级分类下钻清除冲突的一级条件，保留日期与群范围", async () => {
  const user = userEvent.setup();
  const event = dataset.events.find((item) => item.level1 !== "未归类")!;
  await mount(
    "events",
    `?from=2026-08-25&to=2026-08-31&room=${event.roomid}&l1=${encodeURIComponent(event.level1)}`,
  );
  await user.click(screen.getByRole("radio", { name: "二级分类" }).closest("label")!);
  const rank = screen.getByRole("table");
  const target = new URL(
    within(rank).getAllByRole("link")[0]!.getAttribute("href")!,
    "http://localhost",
  );
  expect(target.pathname).toBe("/detail");
  expect(target.searchParams.get("room")).toBe(event.roomid);
  expect(target.searchParams.get("from")).toBe("2026-08-25");
  expect(target.searchParams.get("l1")).toBeNull();
  expect(target.searchParams.get("l2")).toBeTruthy();
});

it("空结果保留筛选区与重置入口", async () => {
  const user = userEvent.setup();
  await mount("events", "?q=不存在的关键词000");
  expect(screen.getByLabelText("事件洞察筛选条件")).toBeInTheDocument();
  expect(screen.getByText("当前范围内没有事件")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "清空筛选" }));
  expect(screen.getByRole("heading", { name: "分类分析" })).toBeInTheDocument();
});

it("一级分类下钻保留二级筛选，无响应入口保留积压范围", async () => {
  const event = dataset.events.find((item) => item.level1 !== "未归类")!;
  const unanswered = {
    ...event,
    occurred_on: "2026-08-25",
    asker_role: "EXTERNAL" as const,
    first_agent_reply_time: null,
    firstReplySec: null,
    first_responder: null,
  };
  const view = await mount(
    "events",
    `?from=2026-08-25&to=2026-08-31&l2=${event.event_type}&status=backlog&overdue=1`,
    { ...dataset, events: [unanswered] },
  );
  const categoryLink = view.container.querySelector(".ev-category-link")!;
  const target = new URL(categoryLink.getAttribute("href")!, "http://localhost");
  expect(target.searchParams.get("l2")).toBe(event.event_type);
  expect(target.searchParams.get("l1")).toBe(event.level1);
  const unresolved = new URL(
    screen.getByRole("link", { name: /无响应 1 起/ }).getAttribute("href")!,
    "http://localhost",
  );
  expect(unresolved.searchParams.get("status")).toBe("backlog");
  expect(unresolved.searchParams.get("overdue")).toBe("1");
  expect(unresolved.searchParams.get("to")).toBe("2026-08-31");
});

it("分类构成包含平台事件，首响样本与无响应率仅用商家事件", async () => {
  const original = dataset.events.find(
    (item) => item.asker_role === "EXTERNAL" && item.firstReplySec !== null,
  )!;
  const data = {
    ...dataset,
    events: [
      original,
      {
        ...original,
        id: original.id + 10000,
        first_agent_reply_time: null,
        firstReplySec: null,
        first_responder: null,
        agents: [],
      },
      { ...original, id: original.id + 20000, asker_role: "INTERNAL" as const, firstReplySec: 0 },
    ],
  };
  const view = await mount("events", "", data);
  const row = view.container.querySelector(".ev-table .ant-table-tbody > tr[data-row-key]")!;
  expect(row).toHaveTextContent("100.0%");
  expect(row).toHaveTextContent("50.0%");
  expect(row).toHaveTextContent("1 / 2 起商家事件");
  expect(screen.getByRole("columnheader", { name: "事件总数" })).toBeVisible();
  expect(screen.getByRole("columnheader", { name: "无响应事件数" })).toBeVisible();
  expect(screen.getByRole("columnheader", { name: "已回复样本" })).toBeVisible();
  view.unmount();
  const platform = data.events[2]!;
  const platformView = await mount("events", "", { ...dataset, events: [platform] });
  const platformRow = platformView.container.querySelector(
    ".ev-table .ant-table-tbody > tr[data-row-key]",
  )!;
  expect(platformRow).toHaveTextContent("0 / 0 起商家事件");
  expect(platformRow).not.toHaveTextContent("0 秒");
  expect(platformView.container.querySelector(".ev-risk-link")).toBeNull();
});

it("未归类指标包括词表外编码，归属与下钻保持一致", async () => {
  const original = dataset.events[0]!;
  const events = decorate([{ ...original, event_type: "unknown_type" }], taxIndex);
  const view = await mount("events", "", { ...dataset, events });
  const metric = screen.getByText("未归类事件", { exact: true }).closest(".od-metric")!;
  expect(metric).toHaveTextContent("1");
  expect(metric).toHaveTextContent("100.0%");
  expect(screen.queryByRole("region", { name: "分类覆盖" })).not.toBeInTheDocument();
  expect(view.container.querySelector(".ev-category-link")).toHaveTextContent("未归类");
  const target = new URL(metric.querySelector("a")!.getAttribute("href")!, "http://localhost");
  expect(target.searchParams.get("l1")).toBe("未归类");
});

it("keeps_trend_gaps_daily_values_and_category_navigation", async () => {
  const user = userEvent.setup();
  const view = await mount("events");
  await user.click(screen.getByRole("tab", { name: "每日趋势" }));
  expect(screen.getByText("各分类独立刻度 · 仅比较走势")).toBeVisible();
  expect(screen.queryByText("分类日走势")).toBeNull();
  expect(screen.queryByText("悬停查看每日事件量")).toBeNull();
  expect(view.container.querySelector(".ev-trend-context")).toHaveTextContent(
    `${dataset.meta.days.length} 天`,
  );
  const failedDay = dataset.groupDaily.find((day) => day.extraction_status === "failed")!.dt;
  const firstTrend = screen.getAllByRole("img", { name: /每日事件量/ })[0]!;
  expect(firstTrend.getAttribute("aria-label")).toContain(failedDay + " 数据不完整");
  const series = JSON.parse(firstTrend.getAttribute("data-series")!) as [
    { name: string; data: (number | null)[]; connectNulls: boolean },
  ];
  const days = dataset.meta.days;
  expect(series[0].data[days.indexOf(failedDay)]).toBeNull();
  expect(series[0].connectNulls).toBe(false);
  const trend = view.container.querySelector(".ev-trend")!;
  const daily = within(trend as HTMLElement).getByText("逐日数据");
  await user.click(daily);
  expect(daily.closest("details")).toHaveAttribute("open");
  const failedRow = within(trend as HTMLElement)
    .getByText(failedDay, { exact: false })
    .closest("dt")!.parentElement!;
  expect(failedRow).toHaveTextContent("数据不完整");
  const link = within(trend as HTMLElement).getByRole("link");
  const target = new URL(link.getAttribute("href")!, "http://localhost");
  expect(target.pathname).toBe("/detail");
  expect(target.searchParams.get("l1")).toBe(series[0].name);
  await user.click(screen.getByRole("radio", { name: "二级分类" }).closest("label")!);
  const types = new Set(dataset.events.map((event) => event.event_type));
  expect(view.container.querySelectorAll(".ev-trend")).toHaveLength(types.size);
  await user.click(screen.getByRole("tab", { name: "构成与响应" }));
  expect(screen.getByRole("columnheader", { name: "二级分类" })).toBeVisible();
});

it("客服页按摘要搜索仍保留参与客服，个人筛选不混入协作者", async () => {
  const user = userEvent.setup();
  const event = dataset.events.find((item) => item.agents.length > 1)!;
  const data = { ...dataset, events: [event] };
  const agent = event.agents[0]!;
  const label = dataset.meta.agents.find((item) => item.agent === agent)?.alias ?? agent;
  const view = await mount(
    "agents",
    `?q=${encodeURIComponent(event.summary)}&agent=${agent}`,
    data,
  );
  const person = screen.getByRole("button", { name: label });
  expect(view.container.querySelectorAll(".ia-table-link")).toHaveLength(1);
  expect(person).toHaveAttribute("title", `${label}\n客服 ID：${agent}`);
  expect(view.container.querySelector(".ag-identity .c2e-sub")).toBeNull();
  await user.click(person);
  const dialog = await screen.findByRole("dialog");
  expect(dialog.querySelector(".od-drawer-id")).toHaveTextContent(agent);
  expect(dialog).toHaveTextContent("每日变化");
  expect(dialog).toHaveTextContent("活跃群明细");
  const target = new URL(
    within(dialog).getByRole("link", { name: "事件明细" }).getAttribute("href")!,
    "http://localhost",
  );
  expect(target.searchParams.get("q")).toBe(event.summary);
  expect(target.searchParams.get("agent")).toBe(agent);
  expect(target.searchParams.has("focus")).toBe(false);
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  expect(screen.getByTestId("url")).toHaveTextContent(`agent=${agent}`);
});

it("客服页默认分组对照，保留样本分母，图表按需切换", async () => {
  const user = userEvent.setup();
  const view = await mount("agents");
  expect(screen.getByRole("tab", { name: "指标明细" })).toHaveAttribute("aria-selected", "true");
  expect(screen.getByRole("columnheader", { name: "参与工作量" })).toBeVisible();
  expect(screen.getByRole("columnheader", { name: /^参与事件数/ })).toBeVisible();
  expect(screen.getByRole("columnheader", { name: /^首响归属事件数/ })).toBeVisible();
  expect(screen.getByRole("columnheader", { name: /^首响统计样本数/ })).toBeVisible();
  expect(screen.queryByRole("columnheader", { name: "解决事件数" })).toBeNull();
  expect(screen.queryByRole("columnheader", { name: "回复消息数" })).toBeNull();
  await user.click(screen.getByRole("tab", { name: "工作量与时效" }));
  expect(screen.getByRole("img", { name: /客服工作量/ })).toBeVisible();
  await user.click(screen.getByRole("tab", { name: "指标明细" }));
  await user.click(view.container.querySelector<HTMLButtonElement>(".ia-table-link")!);
  const dialog = await screen.findByRole("dialog");
  expect(view.container.contains(dialog)).toBe(false);
  expect(within(dialog).queryByRole("columnheader", { name: "无响应" })).toBeNull();
  expect(within(dialog).getByRole("columnheader", { name: "有效首响样本" })).toBeVisible();
});

it("响应构成下钻保持日期、群和客服，积压条件不扩大为全部无响应", async () => {
  const original = dataset.events.find((event) => event.agents.length > 0)!;
  const agent = original.agents[0]!;
  const unanswered = {
    ...original,
    occurred_on: "2026-08-25",
    asker_role: "EXTERNAL" as const,
    first_agent_reply_time: null,
    firstReplySec: null,
    first_responder: null,
  };
  const query = `?from=2026-08-25&to=2026-08-31&room=${original.roomid}&agent=${agent}&status=backlog&overdue=1`;
  await mount("agents", query, { ...dataset, events: [unanswered] });
  const response = screen.getByRole("region", { name: "关键指标" });
  const metric = within(response).getByText("无响应", { exact: true }).closest(".od-metric")!;
  const link = metric.querySelector("a")!;
  expect(link).toHaveTextContent("1起");
  const target = new URL(link.getAttribute("href")!, "http://localhost");
  expect(target.pathname).toBe("/detail");
  expect(target.searchParams.get("agent")).toBe(agent);
  expect(target.searchParams.get("room")).toBe(original.roomid);
  expect(target.searchParams.get("from")).toBe("2026-08-25");
  expect(target.searchParams.get("to")).toBe("2026-08-31");
  expect(target.searchParams.get("status")).toBe("backlog");
  expect(target.searchParams.get("overdue")).toBe("1");
  for (const label of ["按时回复", "超时回复"]) {
    expect(
      within(response).getByText(label, { exact: true }).closest(".od-metric")!.querySelector("a"),
    ).toBeNull();
  }
});

it("仅平台事件不伪造首响，零样本客服仍可查看工作量", async () => {
  const user = userEvent.setup();
  const event = dataset.events.find((item) => item.asker_role === "INTERNAL")!;
  await mount("agents", "", { ...dataset, events: [event] });
  const summary = screen.getByRole("region", { name: "关键指标" });
  expect(summary).toHaveTextContent("0 起商家已回复样本");
  expect(summary).not.toHaveTextContent("0 秒");
  await user.click(screen.getByRole("tab", { name: "工作量与时效" }));
  expect(screen.getByText("没有可比较的客服")).toBeVisible();
  const response = screen.getByRole("region", { name: "关键指标" });
  expect(within(response).getAllByRole("link")).toHaveLength(1);
});

it("追溯默认展示20条，抽屉跨页连续浏览并保留筛选", async () => {
  const user = userEvent.setup();
  const view = await mount("detail", "?from=2026-08-25&to=2026-08-31");
  const rows = view.container.querySelectorAll<HTMLButtonElement>(".ia-summary-link");
  expect(rows).toHaveLength(20);
  expect(view.container.querySelector(".ant-breadcrumb")).toBeNull();
  const lastLabel = rows[19]!.getAttribute("aria-label")!;
  const lastId = Number(lastLabel.match(/#(\d+)/)![1]);
  await user.click(rows[19]!);
  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByLabelText("事件关键数据")).toBeInTheDocument();
  await user.click(within(dialog).getByRole("button", { name: "下一条事件" }));
  expect(screen.getByTestId("url")).toHaveTextContent("page=2");
  expect(screen.getByTestId("url")).toHaveTextContent("from=2026-08-25");
  const firstId = Number(
    view.container
      .querySelector(".ia-summary-link")!
      .getAttribute("aria-label")!
      .match(/#(\d+)/)![1],
  );
  expect(screen.getByTestId("url")).toHaveTextContent(`drawer=${firstId}`);
  await user.click(within(dialog).getByRole("button", { name: "上一条事件" }));
  expect(screen.getByTestId("url")).toHaveTextContent(`drawer=${lastId}`);
  expect(screen.getByTestId("url")).not.toHaveTextContent("page=2");
});

it("追溯合并全部字段，抽屉独立挂载，资料页签不挤占消息区", async () => {
  const user = userEvent.setup();
  const view = await mount("detail");
  expect(screen.queryByText("业务视图")).toBeNull();
  expect(screen.queryByText("审计字段")).toBeNull();
  for (const name of ["一级分类", "二级分类", "活跃客服", "首响时间", "首响耗时", "状态", "尾部"]) {
    expect(screen.getByRole("columnheader", { name })).toBeInTheDocument();
  }
  await user.click(view.container.querySelector<HTMLButtonElement>(".ia-summary-link")!);
  const dialog = await screen.findByRole("dialog");
  expect(view.container.contains(dialog)).toBe(false);
  expect(within(dialog).getByRole("tab", { name: /消息时间线/ })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await user.click(within(dialog).getByRole("tab", { name: "事件资料" }));
  expect(within(dialog).getByRole("heading", { name: "来源与参与方" })).toBeVisible();
  await user.click(within(dialog).getByRole("tab", { name: "统计依据" }));
  expect(within(dialog).getByRole("heading", { name: "统计归属" })).toBeVisible();
  await user.click(within(dialog).getByRole("button", { name: "下一条事件" }));
  expect(within(dialog).getByRole("tab", { name: /消息时间线/ })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
});

/**
 * ⚠️ **越界页码不再夹到最后一页。** 此前那一步 `Math.min` 用的是 `/api/summary`
 * 的事件总数，而明细表翻的是**另一个集合**（不按已知成功群日过滤）—— 窗口里一有
 * 抽取失败的群日，两个数就不等，人被夹在更早的页码上，尾部的行永远翻不到，
 * 而页面看起来一切正常。现在总数由 `/api/events` 自己回，越界就是空表
 * （真接口越过页数返回空数组，不是错误），分页控件照旧带着真实总数，点得回去。
 */
it("越界页码显示空表而不是报错，分页照旧带着真实总数", async () => {
  const first = await mount("detail", "?size=25");
  expect(first.container.querySelectorAll(".ia-summary-link").length).toBeGreaterThan(0);
  const total = first.container.querySelector(".ant-pagination-total-text")!.textContent;
  cleanup();
  const beyond = await mount("detail", "?page=99&size=25");
  expect(beyond.container.querySelectorAll(".ia-summary-link")).toHaveLength(0);
  expect(beyond.container.querySelector(".ant-pagination-total-text")).toHaveTextContent(
    total.split(" / ")[1]!,
  );
  cleanup();
  // 越过后端的翻页护栏（`MAX_PAGE` = 200）时**不能把参数原样发上去** —— 真接口对
  // `page > 200` 返回 400，整页会变成 ErrorState，而这只是一次手改 URL。
  // 模拟数据源不会 400，所以这条只能盯「请求出去的页码」，盯不了渲染结果。
  pageCalls.length = 0;
  const clamped = await mount("detail", "?page=999&size=25");
  expect(clamped.container.querySelectorAll(".ia-summary-link")).toHaveLength(0);
  expect(Math.max(...pageCalls)).toBeLessThanOrEqual(200);
});

it("开关抽屉保留当前页码", async () => {
  const user = userEvent.setup();
  const view = await mount("detail", "?page=2&size=25");
  expect(view.container.querySelectorAll(".ia-summary-link").length).toBeGreaterThan(0);
  await user.click(view.container.querySelector<HTMLButtonElement>(".ia-summary-link")!);
  expect(await screen.findByRole("dialog")).toBeInTheDocument();
  expect(screen.getByTestId("url")).toHaveTextContent("page=2");
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  expect(screen.getByTestId("url")).not.toHaveTextContent("drawer=");
  expect(screen.getByTestId("url")).toHaveTextContent("page=2");
});

it("事件抽屉按表格顺序前后切换，原文为空时显式展示空态", async () => {
  const user = userEvent.setup();
  const view = await mount("detail", "?size=25");
  await user.click(screen.getByRole("columnheader", { name: "开始时间" }));
  await user.click(screen.getByRole("columnheader", { name: "开始时间" }));
  const firstTwo = [...view.container.querySelectorAll<HTMLButtonElement>(".ia-summary-link")]
    .slice(0, 2)
    .map((button) => Number(button.getAttribute("aria-label")?.match(/#(\d+)/)?.[1]));
  await user.click(view.container.querySelector<HTMLButtonElement>(".ia-summary-link")!);
  const dialog = await screen.findByRole("dialog");
  expect(dialog).toHaveTextContent("暂无消息原文");
  expect(within(dialog).getByRole("button", { name: "上一条事件" })).toBeDisabled();
  await user.click(within(dialog).getByRole("button", { name: "下一条事件" }));
  expect(screen.getByTestId("url")).toHaveTextContent(`drawer=${firstTwo[1]}`);
  await user.click(within(dialog).getByRole("button", { name: "上一条事件" }));
  expect(screen.getByTestId("url")).toHaveTextContent(`drawer=${firstTwo[0]}`);
});

it("loads_outside_filter_events_and_reports_missing_ids", async () => {
  const event = dataset.events[0]!;
  vi.stubGlobal(
    "fetch",
    vi.fn((url: URL) =>
      Promise.resolve(
        url.pathname === `/api/event/${event.id}`
          ? Response.json(event)
          : new Response(null, { status: 404 }),
      ),
    ),
  );
  const view = await mount("detail", `?q=不存在的关键词000&drawer=${event.id}`);
  await waitFor(() =>
    expect(screen.getByRole("dialog")).toHaveTextContent("这条事件不在当前筛选结果里"),
  );
  expect(screen.getByRole("button", { name: "下一条事件" })).toBeDisabled();
  view.unmount();
  await mount("detail", "?drawer=999999999");
  await waitFor(() =>
    expect(screen.getByRole("dialog")).toHaveTextContent("找不到事件 #999999999"),
  );
});

it("原文加载失败保留真实原因和重试入口", async () => {
  const user = userEvent.setup();
  messages.mode = "error";
  await mount("detail", `?drawer=${dataset.events[0]!.id}`);
  const dialog = screen.getByRole("dialog");
  expect(dialog).toHaveTextContent("原文服务暂不可用");
  await user.click(within(dialog).getByRole("button", { name: "重试消息原文" }));
  expect(messages.retry).toHaveBeenCalledOnce();
});

it("消息按时间排序，首响标记准确，消息标识可按需展开", async () => {
  const user = userEvent.setup();
  const event = dataset.events.find(
    (item) => item.asker_role === "EXTERNAL" && item.first_responder && item.crossDay,
  )!;
  const source = raw.messages.get(event.id)!;
  messages.data = [...source].reverse();
  await mount("detail", `?drawer=${event.id}`);
  const dialog = screen.getByRole("dialog");
  const rendered = dialog.querySelectorAll(".ed-message-text");
  expect([...rendered].map((node) => node.textContent)).toEqual(
    [...source].sort((a, b) => a.at.localeCompare(b.at)).map((item) => item.text),
  );
  expect(dialog.querySelectorAll('[data-anchor="true"]')).toHaveLength(1);
  expect(within(dialog).getByRole("button", { name: "定位首响" })).toBeEnabled();
  expect(dialog.querySelector(".ed-message-id")).toBeNull();
  await user.click(within(dialog).getByRole("checkbox", { name: "消息标识" }));
  expect(dialog.querySelectorAll(".ed-message-id")).toHaveLength(source.length);
  expect(dialog.querySelectorAll(".ed-day").length).toBeGreaterThan(1);
});

it("消息不完整或首响不匹配时保留警告，禁用首响定位", async () => {
  const event = dataset.events.find(
    (item) => item.asker_role === "EXTERNAL" && item.first_responder,
  )!;
  messages.data = raw.messages
    .get(event.id)!
    .filter((message) => message.sender_role !== "INTERNAL");
  await mount("detail", `?drawer=${event.id}`);
  const dialog = screen.getByRole("dialog");
  expect(dialog).toHaveTextContent("条来源消息");
  expect(dialog).toHaveTextContent("原文中未找到匹配的首响锚点");
  expect(within(dialog).getByRole("button", { name: "定位首响" })).toBeDisabled();
});

it("加载状态仍可核查统计依据，平台发起不标成客服首响", async () => {
  const user = userEvent.setup();
  const event = dataset.events.find((item) => item.asker_role === "INTERNAL")!;
  messages.mode = "loading";
  const view = await mount("detail", `?drawer=${event.id}`);
  expect(screen.getByRole("status", { name: "正在加载消息原文" })).toBeVisible();
  await user.click(screen.getByRole("tab", { name: "统计依据" }));
  expect(screen.getByText("不纳入首响指标")).toBeVisible();
  view.unmount();
  messages.mode = "ready";
  messages.data = raw.messages.get(event.id)!;
  await mount("detail", `?drawer=${event.id}`);
  expect(screen.getByRole("button", { name: "定位首响" })).toBeDisabled();
  expect(screen.queryByText("首次有效回复")).toBeNull();
  expect(screen.getByText("平台推送")).toBeVisible();
});

/**
 * ⚠️ **翻页要真的翻得动。** antd 的 `Table.onChange` 对**每一种**表格变化都会触发，
 * 分页也算（`extra.action === "paginate"`）。明细页那个 handler 只认排序，
 * 却没拦住其它 action —— 点第 2 页时它跟着跑一遍，把 `page` 复位成 1，
 * 而排序本身看起来一切正常。群 / 客服两张表早就用 `action === "sort"` 拦过了。
 */
it("detail_paginates_and_changes_page_size", async () => {
  const user = userEvent.setup();
  const view = await mount("detail");
  const firstPage = [...view.container.querySelectorAll(".ia-summary-link")].map((row) =>
    row.getAttribute("aria-label"),
  );
  expect(firstPage).toHaveLength(20);
  await user.click(view.container.querySelector<HTMLElement>(".ant-pagination-item-2")!);
  expect(screen.getByTestId("url")).toHaveTextContent("page=2");
  const secondPage = [...view.container.querySelectorAll(".ia-summary-link")].map((row) =>
    row.getAttribute("aria-label"),
  );
  expect(secondPage.length).toBeGreaterThan(0);
  expect(secondPage.every((label) => !firstPage.includes(label))).toBe(true);
  // 排序仍要复位页码 —— 拦 action 不能把这件事一起拦掉。
  await user.click(screen.getByRole("columnheader", { name: "开始时间" }));
  expect(screen.getByTestId("url")).toHaveTextContent("sort=time");
  expect(screen.getByTestId("url")).not.toHaveTextContent("page=2");
});
