import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation } from "react-router-dom";
import { afterEach, beforeAll, expect, it, vi } from "vitest";
import { buildMockDataset } from "@/api/mock/generator";
import { buildTaxonomyIndex, decorate } from "@/domain/metrics";
import type { LoadedDataset } from "@/api/source";
import { Providers } from "@/app/providers";
import { Workbench } from "@/components/layout/Workbench";
import { useFilters } from "@/features/filters/useFilters";
import { useAnalytics } from "@/features/filters/useAnalytics";
import { RoomsPage } from "./RoomsPage";

vi.mock("@/components/charts/EChart", () => ({
  EChart: ({ ariaLabel }: { ariaLabel: string }) => <div role="img" aria-label={ariaLabel} />,
}));
afterEach(cleanup);
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
const taxIndex = buildTaxonomyIndex(raw.meta.taxonomy, raw.meta.taxonomy_version);
const dataset: LoadedDataset = {
  ...raw,
  events: decorate(raw.events, taxIndex),
  taxIndex,
  source: "mock",
  loadedAt: 0,
  fallbackReason: null,
};

function Harness({ data = dataset }: { data?: LoadedDataset }) {
  const api = useFilters();
  const analytics = useAnalytics(data, api.filters);
  const location = useLocation();
  return (
    <>
      <RoomsPage analytics={analytics} api={api} />
      <output data-testid="url">{location.search}</output>
    </>
  );
}

it("shows_authoritative_room_name_without_placeholder_badge", () => {
  const room = dataset.meta.rooms[0]!;
  const data: LoadedDataset = {
    ...dataset,
    meta: {
      ...dataset.meta,
      rooms: [
        {
          ...room,
          alias: "真实商家群",
          merchant_id: "9007199254740993",
          alias_is_authoritative: true,
        },
      ],
      alias_is_authoritative: false,
    },
  };
  const view = render(
    <MemoryRouter initialEntries={[`/rooms?room=${room.roomid}`]}>
      <Providers>
        <Workbench>
          <Harness data={data} />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  expect(view.container.querySelector(".ra-room-link")).toHaveTextContent("真实商家群");
  expect(screen.queryByText("别名 待补")).not.toBeInTheDocument();
});

it.each(["all", "room", "query", "partial", "missing", "zero"] as const)(
  "summarizes_messages_for_visible_rooms_%s",
  (scope) => {
    const cell = dataset.groupDaily[0]!;
    const room = dataset.meta.rooms.find((row) => row.roomid === cell.roomid)!;
    const other = dataset.meta.rooms.find((row) => row.roomid !== cell.roomid)!;
    const otherDay = dataset.meta.days.find((day) => day !== cell.dt)!;
    const cells = [
      { ...cell, msg_count: 17 },
      { ...cell, roomid: other.roomid, msg_count: 23, extraction_status: "failed" as const },
      { ...cell, dt: otherDay, msg_count: 100 },
    ];
    const data: LoadedDataset = {
      ...dataset,
      meta: { ...dataset.meta, rooms: [room, other] },
      events: [],
      groupDaily:
        scope === "missing"
          ? []
          : scope === "partial"
            ? cells.slice(0, 1)
            : scope === "zero"
              ? cells.map((row) => ({ ...row, msg_count: 0 }))
              : cells,
    };
    const search = new URLSearchParams({ from: cell.dt, to: cell.dt });
    if (scope === "room") search.set("room", room.roomid);
    if (scope === "query") search.set("q", room.roomid);
    const view = render(
      <MemoryRouter initialEntries={[`/rooms?${search}`]}>
        <Providers>
          <Workbench>
            <Harness data={data} />
          </Workbench>
        </Providers>
      </MemoryRouter>,
    );
    const summary = screen.getByLabelText("群指标摘要");
    const expected =
      scope === "missing" ? "—" : scope === "zero" ? "0" : scope === "all" ? "40" : "17";
    expect(summary).toHaveTextContent(`消息总量 ${expected} 条`);
    const roomCount = scope === "room" || scope === "query" ? 1 : 2;
    expect(summary).toHaveTextContent(`${roomCount} 个群`);
    expect(summary).toHaveTextContent(/活跃群.*消息总量.*事件量/);
    expect(summary).toHaveTextContent(`活跃群 ${scope === "missing" ? "—" : "0"} 个`);
    expect(summary).toHaveTextContent(`事件量 ${scope === "missing" ? "—" : "0"} 起`);
    expect(view.container.querySelectorAll(".ra-room-link")).toHaveLength(roomCount);
    if (scope === "partial") expect(summary).toHaveTextContent("仅已知量");
    if (scope === "missing") expect(summary).toHaveTextContent("消息总量暂缺");
    if (scope === "all") expect(summary).not.toHaveTextContent("数据不完整");
  },
);

it("缺记录的群保持可见并标为未知，不声称抽取完整", () => {
  const room = dataset.meta.rooms[0]!;
  const data = {
    ...dataset,
    groupDaily: dataset.groupDaily.filter((r) => r.roomid !== room.roomid),
  };
  const view = render(
    <MemoryRouter initialEntries={[`/rooms?room=${room.roomid}`]}>
      <Providers>
        <Workbench>
          <Harness data={data} />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  expect(view.container.querySelectorAll(".ra-room-link")).toHaveLength(1);
  expect(view.container.textContent).toContain("完整性未知");
  expect(view.container.textContent).not.toContain("当前窗口抽取完整");
  expect(view.container.textContent).toContain("7 日无记录");
});

it("选择一个群后只显示该群指标", () => {
  const room = dataset.meta.rooms[0]!;
  const view = render(
    <MemoryRouter initialEntries={[`/rooms?room=${room.roomid}`]}>
      <Providers>
        <Workbench>
          <Harness />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  expect(view.container.querySelectorAll(".ra-room-link")).toHaveLength(1);
  expect(view.container.querySelector(".ra-room-link")).toHaveTextContent(room.alias!);
});

it("子路径部署的分类下钻链接包含 basename", () => {
  const view = render(
    <MemoryRouter basename="/board" initialEntries={["/board/rooms?source=mock"]}>
      <Providers>
        <Workbench>
          <Harness />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  const link = view.container.querySelector<HTMLAnchorElement>(".ra-category-link")!;
  expect(new URL(link.href).pathname).toBe("/board/detail");
  expect(new URL(link.href).searchParams.get("source")).toBe("mock");
});

it("群表前置消息数，点击群名打开独立七天指标，关闭保留原筛选，明细入口使用抽屉口径", async () => {
  const user = userEvent.setup();
  const search = "?from=2026-08-31&to=2026-08-31&status=unreplied";
  const view = render(
    <MemoryRouter initialEntries={[`/rooms${search}`]}>
      <Providers>
        <Workbench>
          <Harness />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  const headers = [...view.container.querySelectorAll(".ra-details .ant-table-thead th")].map(
    (cell) => cell.textContent,
  );
  expect(headers.slice(0, 3)).toEqual(["群", "消息总量", "事件量"]);
  expect(headers).toContain("主要事件类型");
  expect(headers.slice(4, 9)).toEqual(["商家发起", "首响 P50", "首响 P90", "无响应", "无响应率"]);
  expect(screen.getByRole("heading", { name: "群聊洞察" })).toBeInTheDocument();
  expect(headers).not.toContain("每日趋势");
  const button = view.container.querySelector<HTMLButtonElement>(".ra-room-link")!;
  await user.click(button);
  const dialog = await screen.findByRole("dialog");
  expect(dialog).toHaveTextContent("2026-08-25 至 2026-08-31");
  for (const name of ["消息总量", "事件量", "事件类型分布", "首响指标"]) {
    expect(within(dialog).getByRole("heading", { name })).toBeInTheDocument();
  }
  expect(within(dialog).getAllByRole("img", { name: /近7天/ })).toHaveLength(4);
  expect(screen.getByTestId("url")).toHaveTextContent(search);
  const href = within(dialog).getByRole("link", { name: "事件明细" }).getAttribute("href")!;
  const params = new URL(href, "http://localhost").searchParams;
  expect(params.get("from")).toBe("2026-08-25");
  expect(params.get("to")).toBe("2026-08-31");
  expect(params.get("status")).toBeNull();
  expect(params.get("room")).toBeTruthy();
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  expect(screen.getByTestId("url")).toHaveTextContent(search);
});

it("主要事件类型渲染四个计数标签，第四项保留群与类型的下钻条件", async () => {
  const user = userEvent.setup();
  const view = render(
    <MemoryRouter initialEntries={["/rooms?from=2026-08-25&to=2026-08-31"]}>
      <Providers>
        <Workbench>
          <Harness />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  const tags = view.container.querySelector<HTMLElement>(".ra-category-tags")!;
  const links = within(tags).getAllByRole("link");
  expect(links).toHaveLength(4);
  for (const link of links) {
    expect(link.querySelector("span")).not.toBeEmptyDOMElement();
    expect(Number(link.querySelector("b")?.textContent)).toBeGreaterThan(0);
  }
  const fourth = links[3]!;
  const target = new URL(fourth.getAttribute("href")!, "http://localhost");
  expect(target.pathname).toBe("/detail");
  expect(target.searchParams.get("room")).toBeTruthy();
  expect(target.searchParams.get("l1")).toBe(fourth.querySelector("span")?.textContent);
  expect(target.searchParams.get("from")).toBe("2026-08-25");
  expect(target.searchParams.get("to")).toBe("2026-08-31");
  await user.click(fourth);
  expect(screen.getByTestId("url")).toHaveTextContent(target.search);
  expect(screen.queryByRole("dialog")).toBeNull();
});
