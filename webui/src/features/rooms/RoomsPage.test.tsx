import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation } from "react-router-dom";
import { afterEach, beforeAll, expect, it, vi } from "vitest";
import { buildMockDataset } from "@/test/mock/generator";
import { buildTaxonomyIndex, decorate } from "@/domain/metrics";
import type { LoadedDataset } from "@/api/source";
import type { DecoratedEvent, RoomCategoryAgg } from "@/domain/schemas";
import { Providers } from "@/app/providers";
import { queryClient } from "@/app/queryClient";
import { Workbench } from "@/components/layout/Workbench";
import { useFilters } from "@/features/filters/useFilters";
import { useAnalytics } from "@/features/filters/useAnalytics";
import type * as ExportRooms from "./exportRooms";
import { RoomsPage } from "./RoomsPage";

vi.mock("@/components/charts/EChart", () => ({
  EChart: ({ ariaLabel }: { ariaLabel: string }) => <div role="img" aria-label={ariaLabel} />,
}));
/**
 * 页面的指标现在全部来自聚合接口，所以视图测试摆布的是**这批事件**，
 * 由 `mock/aggregate`（口径的前端对照实现）算成接口的形状 —— 不手写假数字。
 */
const stub = vi.hoisted(() => ({ events: [], groupDaily: [], tax: new Map() }) as never);
// 群 × 类型只有导出会取，聚合替身不管它：这里给一个可指定返回值的替身。
const roomCategories = vi.hoisted(() =>
  vi.fn<(f: unknown) => Promise<RoomCategoryAgg[]>>(() => Promise.resolve([])),
);
vi.mock("@/api/source", async (importOriginal) => {
  const { sourceStub } = await import("@/test/aggregateStub");
  return {
    ...sourceStub(await importOriginal(), stub),
    loadRoomCategories: roomCategories,
  };
});
// 下载本身（浏览器里写文件）替换掉，只验证页面交给它的那两张表。
const download = vi.hoisted(() =>
  vi.fn<(sheets: { metrics: unknown[]; types: unknown[][] }) => Promise<void>>(),
);
vi.mock("./exportRooms", async (importOriginal) => ({
  ...(await importOriginal<typeof ExportRooms>()),
  downloadRoomSheets: download,
}));
afterEach(cleanup);
// 缓存是模块级单例，用例之间不清就会读到上一个用例的数字。
afterEach(() => queryClient.clear());
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
  ...raw,
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
    rooms: data.meta.rooms,
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

function Harness({ data = dataset }: { data?: TestDataset }) {
  const api = useFilters();
  const analytics = useTestAnalytics(data, api.filters);
  const location = useLocation();
  return (
    <>
      <RoomsPage analytics={analytics} api={api} />
      <output data-testid="url">{location.search}</output>
    </>
  );
}

it("shows_merchant_name_and_falls_back_to_merchant_id", async () => {
  const room = dataset.meta.rooms[0]!;
  const withMerchant = (extra: Partial<(typeof dataset.meta.rooms)[number]>) => ({
    ...dataset,
    meta: {
      ...dataset.meta,
      rooms: [{ ...room, alias: "真实商家群", alias_is_authoritative: true, ...extra }],
      alias_is_authoritative: false,
    },
  });
  const show = async (data: TestDataset) => {
    const view = render(
      <MemoryRouter initialEntries={[`/rooms?room=${room.roomid}`]}>
        <Providers>
          <Workbench>
            <Harness data={data} />
          </Workbench>
        </Providers>
      </MemoryRouter>,
    );
    await settle(view);
    return view;
  };

  // ① 商家摘要表里有店铺名 —— 群名下面挂商家名。
  const resolved = await show(
    withMerchant({
      merchant_id: "42",
      merchant_name: "甲商家",
      merchant_name_is_authoritative: true,
    }),
  );
  expect(resolved.container.querySelector(".ra-room-merchant")).toHaveTextContent("甲商家");
  resolved.unmount();

  // ② 关联了商家但表里没有名字 —— 回落显示商家 ID，**不是空白**。
  const unresolved = await show(
    withMerchant({ merchant_id: "42", merchant_name: null, merchant_name_is_authoritative: false }),
  );
  expect(unresolved.container.querySelector(".ra-room-merchant")).toHaveTextContent("42");
  unresolved.unmount();

  // ③ 压根没关联商家 —— 那一块整个不渲染，不出现空白或 undefined。
  const none = await show(withMerchant({ merchant_id: null, merchant_name: null }));
  expect(none.container.querySelector(".ra-room-merchant")).toBeNull();
  expect(none.container).not.toHaveTextContent("undefined");
  none.unmount();
});

it("shows_authoritative_room_name_without_placeholder_badge", async () => {
  const room = dataset.meta.rooms[0]!;
  const data: TestDataset = {
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
  await settle(view);
  expect(view.container.querySelector(".ra-room-link")).toHaveTextContent("真实商家群");
  expect(view.container.querySelector(".ra-details .c2e-sub")).toBeNull();
  expect(view.container.querySelector(".ra-details .ant-table-tbody")).not.toHaveTextContent(
    room.roomid,
  );
  expect(screen.queryByText("别名 待补")).not.toBeInTheDocument();
});

it.each(["all", "room", "partial", "missing", "zero"] as const)(
  "summarizes_messages_for_visible_rooms_%s",
  async (scope) => {
    const cell = dataset.groupDaily[0]!;
    const room = dataset.meta.rooms.find((row) => row.roomid === cell.roomid)!;
    const other = dataset.meta.rooms.find((row) => row.roomid !== cell.roomid)!;
    const otherDay = dataset.meta.days.find((day) => day !== cell.dt)!;
    const cells = [
      { ...cell, msg_count: 17 },
      { ...cell, roomid: other.roomid, msg_count: 23, extraction_status: "failed" as const },
      { ...cell, dt: otherDay, msg_count: 100 },
    ];
    const data: TestDataset = {
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
    const view = render(
      <MemoryRouter initialEntries={[`/rooms?${search}`]}>
        <Providers>
          <Workbench>
            <Harness data={data} />
          </Workbench>
        </Providers>
      </MemoryRouter>,
    );
    await settle(view);
    const summary = screen.getByLabelText("群指标摘要");
    const expected =
      scope === "missing" ? "—" : scope === "zero" ? "0" : scope === "all" ? "40" : "17";
    expect(summary).toHaveTextContent(`消息总量 ${expected} 条`);
    const roomCount = scope === "room" ? 1 : 2;
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

it("缺记录的群保持可见并标为未知，不声称抽取完整", async () => {
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
  await settle(view);
  expect(view.container.querySelectorAll(".ra-room-link")).toHaveLength(1);
  expect(view.container.textContent).toContain("完整性未知");
  expect(view.container.textContent).not.toContain("当前窗口抽取完整");
  expect(view.container.textContent).toContain("7 日无记录");
});

it("选择一个群后只显示该群指标", async () => {
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
  await settle(view);
  expect(view.container.querySelectorAll(".ra-room-link")).toHaveLength(1);
  expect(view.container.querySelector(".ra-room-link")).toHaveTextContent(room.alias!);
});

it("keyword_matches_only_event_summary_and_hides_rooms_without_matching_events", async () => {
  const [a, b, c] = dataset.meta.rooms as [RoomOption, RoomOption, RoomOption];
  const eventOf = (roomid: string, summary: string) => ({
    ...dataset.events.find((event) => event.roomid === roomid)!,
    summary,
  });
  const data: TestDataset = {
    ...dataset,
    meta: { ...dataset.meta, rooms: [a, b, { ...c, alias: "退款专线群" }] },
    events: [eventOf(a.roomid, "客户申请退款"), eventOf(b.roomid, "催促发货")],
  };
  const view = render(
    <MemoryRouter initialEntries={["/rooms?q=退款"]}>
      <Providers>
        <Workbench>
          <Harness data={data} />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  await settle(view);
  const labels = [...view.container.querySelectorAll(".ra-room-link")].map((el) => el.textContent);
  expect(labels).toEqual([a.alias]);
});

type RoomOption = TestDataset["meta"]["rooms"][number];
it.each<[string, (r: RoomOption) => boolean]>([
  ["group=华东组", (r) => r.merchant_group_config_name === "华东组"],
  ["manager=1003", (r) => r.business_manager_id === "1003"],
  [
    "group=华南组&manager=1002",
    (r) => r.merchant_group_config_name === "华南组" && r.business_manager_id === "1002",
  ],
])("商家分组与业务经理筛选收敛群列表，指标只看这些群（%s）", async (search, pick) => {
  const expected = dataset.meta.rooms.filter(pick);
  expect(expected.length).toBeGreaterThan(0);
  expect(expected.length).toBeLessThan(dataset.meta.rooms.length);
  const view = render(
    <MemoryRouter initialEntries={[`/rooms?${search}`]}>
      <Providers>
        <Workbench>
          <Harness />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  await settle(view);
  const labels = [...view.container.querySelectorAll(".ra-room-link")].map((el) => el.textContent);
  expect(labels.sort()).toEqual(expected.map((r) => r.alias).sort());
  expect(screen.getByLabelText("群指标摘要")).toHaveTextContent(`${expected.length} 个群`);
});

it("子路径部署的分类下钻链接包含 basename", async () => {
  const view = render(
    <MemoryRouter basename="/board" initialEntries={["/board/rooms?source=mock"]}>
      <Providers>
        <Workbench>
          <Harness />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  await settle(view);
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
  await settle(view);
  const headers = [...view.container.querySelectorAll(".ra-details .ant-table-thead th")].map(
    (cell) => cell.textContent,
  );
  expect(headers.slice(0, 5)).toEqual(["群", "商家分组", "业务经理", "消息总量", "事件量"]);
  expect(headers).toContain("主要事件类型");
  expect(headers.slice(6, 11)).toEqual(["商家发起", "首响 P50", "首响 P90", "无响应", "无响应率"]);
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
  await settle(view);
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

/** 四个群覆盖三条显示规则：有值 · 经理只有编号 · 「未分组」原样 · 没关联商家。 */
const attributionRooms = [
  { alias: "群甲", group: "未分组", managerId: null, managerName: null },
  { alias: "群乙", group: "华东组", managerId: "1001", managerName: "李经理" },
  { alias: "群丙", group: "华南组", managerId: "9007199254740993", managerName: null },
  { alias: "群丁", group: null, managerId: null, managerName: null },
];
const attributionData: TestDataset = {
  ...dataset,
  meta: {
    ...dataset.meta,
    rooms: dataset.meta.rooms.slice(0, 4).map((room, i) => ({
      ...room,
      alias: attributionRooms[i]!.alias,
      alias_is_authoritative: true,
      merchant_group_config_name: attributionRooms[i]!.group,
      business_manager_id: attributionRooms[i]!.managerId,
      business_manager_name: attributionRooms[i]!.managerName,
    })),
  },
};

async function renderAttribution() {
  const view = render(
    <MemoryRouter initialEntries={["/rooms"]}>
      <Providers>
        <Workbench>
          <Harness data={attributionData} />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  await settle(view);
  return view;
}

/** 按表头名取每一行的「群 / 商家分组 / 业务经理」三格文字，行序即页面上的顺序。 */
function attributionRows(container: HTMLElement) {
  const heads = [...container.querySelectorAll(".ra-details .ant-table-thead th")].map(
    (th) => th.textContent,
  );
  return [...container.querySelectorAll(".ra-details .ant-table-tbody tr.ant-table-row")].map(
    (tr) => {
      const cells = tr.querySelectorAll("td");
      return {
        room: tr.querySelector(".ra-room-link")!.textContent,
        group: cells[heads.indexOf("商家分组")]!.textContent,
        manager: cells[heads.indexOf("业务经理")]!.textContent,
      };
    },
  );
}

it("shows_merchant_group_and_manager_columns_with_fallbacks", async () => {
  const view = await renderAttribution();
  const byRoom = Object.fromEntries(attributionRows(view.container).map((r) => [r.room, r]));
  // 「未分组」照原样；经理缺失显示 —
  expect(byRoom["群甲"]).toMatchObject({ group: "未分组", manager: "—" });
  // 有姓名用姓名
  expect(byRoom["群乙"]).toMatchObject({ group: "华东组", manager: "李经理" });
  // 只有编号时显示编号
  expect(byRoom["群丙"]).toMatchObject({ group: "华南组", manager: "9007199254740993" });
  // 没关联商家：两项都是 —
  expect(byRoom["群丁"]).toMatchObject({ group: "—", manager: "—" });
});

it.each(["商家分组", "业务经理"] as const)(
  "sorts_by_%s_with_missing_values_always_last",
  async (header) => {
    const user = userEvent.setup();
    const view = await renderAttribution();
    const key = header === "商家分组" ? "group" : "manager";
    const th = () =>
      [...view.container.querySelectorAll(".ra-details .ant-table-thead th")].find(
        (cell) => cell.textContent === header,
      )!;
    const missingFlags = () => attributionRows(view.container).map((r) => r[key] === "—");

    await user.click(th()); // 正序
    expect(th()).toHaveAttribute("aria-sort", "ascending");
    expect(missingFlags()).toEqual(
      key === "group" ? [false, false, false, true] : [false, false, true, true],
    );

    await user.click(th()); // 倒序：缺失值仍在最后
    expect(th()).toHaveAttribute("aria-sort", "descending");
    expect(missingFlags()).toEqual(
      key === "group" ? [false, false, false, true] : [false, false, true, true],
    );
  },
);

it("shows_merchant_group_and_manager_under_the_drawer_title", async () => {
  const user = userEvent.setup();
  const view = await renderAttribution();
  const open = async (alias: string) => {
    const button = [...view.container.querySelectorAll<HTMLButtonElement>(".ra-room-link")].find(
      (el) => el.textContent === alias,
    )!;
    await user.click(button);
    return screen.findByRole("dialog");
  };

  const resolved = await open("群乙");
  expect(within(resolved).getByText(/商家分组/)).toHaveTextContent(
    "商家分组：华东组 · 业务经理：李经理",
  );
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

  const bare = await open("群丙");
  expect(within(bare).getByText(/商家分组/)).toHaveTextContent(
    "商家分组：华南组 · 业务经理：9007199254740993",
  );
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

  const none = await open("群丁");
  expect(within(none).getByText(/商家分组/)).toHaveTextContent("商家分组：— · 业务经理：—");
});

it("exports_every_filtered_room_not_only_the_current_page", async () => {
  const user = userEvent.setup();
  await settle(
    render(
      <MemoryRouter initialEntries={["/rooms?size=10"]}>
        <Providers>
          <Workbench>
            <Harness />
          </Workbench>
        </Providers>
      </MemoryRouter>,
    ),
  );
  const total = Number(/共 (\d+) 个群/.exec(screen.getByText(/共 \d+ 个群/).textContent)![1]);
  expect(total).toBeGreaterThan(10);

  roomCategories.mockResolvedValueOnce([
    { roomid: dataset.meta.rooms[0]!.roomid, key: null, count: 2 },
  ]);
  await user.click(screen.getByRole("button", { name: /导出 Excel/ }));
  await waitFor(() => expect(download).toHaveBeenCalledTimes(1));
  const { metrics, types } = download.mock.calls[0]![0];
  // 摘要 · 空行 · 表头之后，每个群一行 —— 不受当前页大小限制。
  expect(metrics).toHaveLength(3 + total);
  // 第二个 sheet 的数据是点了导出才取的，带着页面当前的筛选；打标未完成作为普通类型行写出。
  expect(roomCategories).toHaveBeenCalledTimes(1);
  expect(types.slice(3).some((r) => r[5] === "打标未完成" && r[6] === 2)).toBe(true);
});
