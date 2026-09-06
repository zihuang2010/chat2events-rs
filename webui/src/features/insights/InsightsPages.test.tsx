import { cleanup, render, screen, within, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation } from "react-router-dom";
import { afterEach, beforeAll, beforeEach, expect, it, vi } from "vitest";
import { buildMockDataset } from "@/api/mock/generator";
import { buildTaxonomyIndex, decorate } from "@/domain/metrics";
import type { LoadedDataset } from "@/api/source";
import { Providers } from "@/app/providers";
import { Workbench } from "@/components/layout/Workbench";
import { useFilters } from "@/features/filters/useFilters";
import { useAnalytics } from "@/features/filters/useAnalytics";
import { EventsPage } from "@/features/events/EventsPage";
import { AgentsPage } from "@/features/agents/AgentsPage";
import { DetailPage } from "@/features/detail/DetailPage";
import type { MessageRow } from "@/domain/schemas";

const messages = vi.hoisted(() => {
  const data: MessageRow[] = [];
  return { mode: "empty", data, retry: vi.fn() };
});
vi.mock("@/api/queries", () => ({
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
  EChart: ({ ariaLabel }: { ariaLabel: string }) => <div role="img" aria-label={ariaLabel} />,
}));
afterEach(cleanup);
beforeEach(() => {
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

const raw = buildMockDataset();
const taxIndex = buildTaxonomyIndex(raw.meta.taxonomy);
const dataset: LoadedDataset = {
  ...raw,
  events: decorate(raw.events, taxIndex),
  taxIndex,
  source: "mock",
  loadedAt: 0,
  fallbackReason: null,
};
const components = { events: EventsPage, agents: AgentsPage, detail: DetailPage };

function Harness({ page, data }: { page: keyof typeof components; data: LoadedDataset }) {
  const api = useFilters();
  const analytics = useAnalytics(data, api.filters);
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
function mount(page: keyof typeof components, search = "", data = dataset) {
  return render(
    <MemoryRouter initialEntries={[`/${page}${search}`]}>
      <Providers>
        <Workbench>
          <Harness page={page} data={data} />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
}

it.each(["events", "agents", "detail"] as const)("%s 全部抽取失败时指标不伪装成零", (page) => {
  const data: LoadedDataset = {
    ...dataset,
    events: [],
    groupDaily: dataset.groupDaily.map((row) => ({ ...row, extraction_status: "failed" })),
  };
  const view = mount(page, "", data);
  const values = Array.from(view.container.querySelectorAll(".ia-metrics .od-metric-value"));
  expect(values).toHaveLength(4);
  expect(values.every((value) => value.textContent.startsWith("—"))).toBe(true);
  expect(view.container.querySelector(".ag-response")).toBeNull();
});

it("二级分类下钻清除冲突的一级条件，保留日期与群范围", async () => {
  const user = userEvent.setup();
  const event = dataset.events.find((item) => item.level1 !== "未归类")!;
  mount(
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
  mount("events", "?q=不存在的关键词000");
  expect(screen.getByLabelText("事件洞察筛选条件")).toBeInTheDocument();
  expect(screen.getByText("当前范围内没有事件")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "清空筛选" }));
  expect(screen.getByRole("heading", { name: "分类分析" })).toBeInTheDocument();
});

it("一级分类下钻保留二级筛选，无响应入口保留积压范围", () => {
  const event = dataset.events.find((item) => item.level1 !== "未归类")!;
  const unanswered = {
    ...event,
    occurred_on: "2026-08-25",
    asker_role: "EXTERNAL" as const,
    first_agent_reply_time: null,
    firstReplySec: null,
    first_responder: null,
  };
  const view = mount(
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

it("分类构成包含平台事件，首响样本与无响应率仅用商家事件", () => {
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
  const view = mount("events", "", data);
  const row = view.container.querySelector(".ev-table .ant-table-tbody > tr[data-row-key]")!;
  expect(row).toHaveTextContent("100.0%");
  expect(row).toHaveTextContent("50.0%");
  expect(row).toHaveTextContent("1 / 2 起商家事件");
  expect(screen.getByRole("columnheader", { name: "已回复样本" })).toBeVisible();
  view.unmount();
  const platform = data.events[2]!;
  const platformView = mount("events", "", { ...dataset, events: [platform] });
  const platformRow = platformView.container.querySelector(
    ".ev-table .ant-table-tbody > tr[data-row-key]",
  )!;
  expect(platformRow).toHaveTextContent("0 / 0 起商家事件");
  expect(platformRow).not.toHaveTextContent("0 秒");
  expect(platformView.container.querySelector(".ev-risk-link")).toBeNull();
});

it("未归类指标包括词表外编码，归属与下钻保持一致", () => {
  const original = dataset.events[0]!;
  const events = decorate([{ ...original, event_type: "unknown_type" }], taxIndex);
  const view = mount("events", "", { ...dataset, events });
  const metric = screen.getByText("未归类事件", { exact: true }).closest(".od-metric")!;
  expect(metric).toHaveTextContent("1");
  expect(metric).toHaveTextContent("100.0%");
  expect(screen.getByRole("region", { name: "分类覆盖" })).toHaveTextContent("已出现 0");
  expect(view.container.querySelector(".ev-category-link")).toHaveTextContent("未归类");
  const target = new URL(metric.querySelector("a")!.getAttribute("href")!, "http://localhost");
  expect(target.searchParams.get("l1")).toBe("未归类");
});

it("每日趋势跟随分类层级，失败日保持缺口", async () => {
  const user = userEvent.setup();
  const view = mount("events");
  await user.click(screen.getByRole("tab", { name: "每日趋势" }));
  expect(screen.getByText("各分类独立刻度 · 仅比较走势")).toBeVisible();
  const failedDay = dataset.groupDaily.find((day) => day.extraction_status === "failed")!.dt;
  const firstTrend = view.container.querySelector('.ev-trend [role="img"]')!;
  expect(firstTrend.getAttribute("aria-label")).toContain(failedDay + " 数据不完整");
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
  const view = mount("agents", `?q=${encodeURIComponent(event.summary)}&agent=${agent}`, data);
  const person = screen.getByRole("button", { name: label });
  expect(view.container.querySelectorAll(".ia-table-link")).toHaveLength(1);
  await user.click(person);
  const dialog = await screen.findByRole("dialog");
  expect(dialog).toHaveTextContent("每日变化");
  expect(dialog).toHaveTextContent("服务群明细");
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
  const view = mount("agents");
  expect(screen.getByRole("tab", { name: "指标明细" })).toHaveAttribute("aria-selected", "true");
  expect(screen.getByRole("columnheader", { name: "参与工作量" })).toBeVisible();
  expect(screen.getByRole("columnheader", { name: "有效样本" })).toBeVisible();
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

it("响应构成下钻保持日期、群和客服，积压条件不扩大为全部无响应", () => {
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
  mount("agents", query, { ...dataset, events: [unanswered] });
  const response = screen.getByRole("region", { name: "事件响应构成" });
  const link = within(response).getByRole("link", { name: /无响应/ });
  expect(link).toHaveTextContent("1起");
  const target = new URL(link.getAttribute("href")!, "http://localhost");
  expect(target.pathname).toBe("/detail");
  expect(target.searchParams.get("agent")).toBe(agent);
  expect(target.searchParams.get("room")).toBe(original.roomid);
  expect(target.searchParams.get("from")).toBe("2026-08-25");
  expect(target.searchParams.get("to")).toBe("2026-08-31");
  expect(target.searchParams.get("status")).toBe("backlog");
  expect(target.searchParams.get("overdue")).toBe("1");
  expect(within(response).queryByRole("link", { name: /按时回复|超时回复|查看事件/ })).toBeNull();
});

it("仅平台事件不伪造首响，零样本客服仍可查看工作量", async () => {
  const user = userEvent.setup();
  const event = dataset.events.find((item) => item.asker_role === "INTERNAL")!;
  mount("agents", "", { ...dataset, events: [event] });
  const summary = screen.getByRole("region", { name: "关键指标" });
  expect(summary).toHaveTextContent("0 起商家已回复样本");
  expect(summary).not.toHaveTextContent("0 秒");
  await user.click(screen.getByRole("tab", { name: "工作量与时效" }));
  expect(screen.getByText("没有可比较的客服")).toBeVisible();
  const response = screen.getByRole("region", { name: "事件响应构成" });
  expect(within(response).getAllByRole("link")).toHaveLength(1);
});

it("追溯默认展示20条，抽屉跨页连续浏览并保留筛选", async () => {
  const user = userEvent.setup();
  const view = mount("detail", "?from=2026-08-25&to=2026-08-31");
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
  const view = mount("detail");
  expect(screen.queryByText("业务视图")).toBeNull();
  expect(screen.queryByText("审计字段")).toBeNull();
  for (const name of [
    "一级分类",
    "二级分类",
    "活跃客服",
    "首响时间",
    "首响耗时",
    "状态",
    "已解决",
  ]) {
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

it("越界页码仍显示最后一页，开关抽屉保留有效页码", async () => {
  const user = userEvent.setup();
  const view = mount("detail", "?page=999&size=25");
  expect(view.container.querySelectorAll(".ia-summary-link").length).toBeGreaterThan(0);
  await user.click(view.container.querySelector<HTMLButtonElement>(".ia-summary-link")!);
  expect(await screen.findByRole("dialog")).toBeInTheDocument();
  expect(screen.getByTestId("url")).toHaveTextContent(
    `page=${Math.ceil(dataset.events.length / 25)}`,
  );
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  expect(screen.getByTestId("url")).not.toHaveTextContent("drawer=");
});

it("事件抽屉按表格顺序前后切换，原文为空时显式展示空态", async () => {
  const user = userEvent.setup();
  const view = mount("detail", "?size=25");
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

it("跨筛选事件直达与不存在的事件分别展示明确状态", () => {
  const event = dataset.events[0]!;
  const view = mount("detail", `?q=不存在的关键词000&drawer=${event.id}`);
  expect(screen.getByRole("dialog")).toHaveTextContent("这条事件不在当前筛选结果里");
  expect(screen.getByRole("button", { name: "下一条事件" })).toBeDisabled();
  view.unmount();
  mount("detail", "?drawer=999999999");
  expect(screen.getByRole("dialog")).toHaveTextContent("找不到事件 #999999999");
});

it("原文加载失败保留真实原因和重试入口", async () => {
  const user = userEvent.setup();
  messages.mode = "error";
  mount("detail", `?drawer=${dataset.events[0]!.id}`);
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
  mount("detail", `?drawer=${event.id}`);
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

it("消息不完整或首响不匹配时保留警告，禁用首响定位", () => {
  const event = dataset.events.find(
    (item) => item.asker_role === "EXTERNAL" && item.first_responder,
  )!;
  messages.data = raw.messages
    .get(event.id)!
    .filter((message) => message.sender_role !== "INTERNAL");
  mount("detail", `?drawer=${event.id}`);
  const dialog = screen.getByRole("dialog");
  expect(dialog).toHaveTextContent("条来源消息");
  expect(dialog).toHaveTextContent("原文中未找到匹配的首响锚点");
  expect(within(dialog).getByRole("button", { name: "定位首响" })).toBeDisabled();
});

it("加载状态仍可核查统计依据，平台发起不标成客服首响", async () => {
  const user = userEvent.setup();
  const event = dataset.events.find((item) => item.asker_role === "INTERNAL")!;
  messages.mode = "loading";
  const view = mount("detail", `?drawer=${event.id}`);
  expect(screen.getByRole("status", { name: "正在加载消息原文" })).toBeVisible();
  await user.click(screen.getByRole("tab", { name: "统计依据" }));
  expect(screen.getByText("不纳入首响指标")).toBeVisible();
  view.unmount();
  messages.mode = "ready";
  messages.data = raw.messages.get(event.id)!;
  mount("detail", `?drawer=${event.id}`);
  expect(screen.getByRole("button", { name: "定位首响" })).toBeDisabled();
  expect(screen.queryByText("首次有效回复")).toBeNull();
  expect(screen.getByText("平台推送")).toBeVisible();
});
