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
const taxIndex = buildTaxonomyIndex(raw.meta.taxonomy);
const dataset: LoadedDataset = {
  ...raw,
  events: decorate(raw.events, taxIndex),
  taxIndex,
  source: "mock",
  loadedAt: 0,
  fallbackReason: null,
};

function Harness() {
  const api = useFilters();
  const analytics = useAnalytics(dataset, api.filters);
  const location = useLocation();
  return (
    <>
      <RoomsPage analytics={analytics} api={api} />
      <output data-testid="url">{location.search}</output>
    </>
  );
}

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
  expect(headers.slice(0, 3)).toEqual(["群", "消息数", "事件数"]);
  expect(headers).toContain("主要事件类型");
  expect(headers.slice(4, 9)).toEqual(["商家发起", "首响 P50", "首响 P90", "无响应", "无响应率"]);
  expect(screen.getByRole("heading", { name: "群聊洞察" })).toBeInTheDocument();
  expect(headers).not.toContain("每日趋势");
  const button = view.container.querySelector<HTMLButtonElement>(".ra-room-link")!;
  await user.click(button);
  const dialog = await screen.findByRole("dialog");
  expect(dialog).toHaveTextContent("2026-08-25 至 2026-08-31");
  for (const name of ["消息指标", "事件指标", "事件类型分布", "首响指标"]) {
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
