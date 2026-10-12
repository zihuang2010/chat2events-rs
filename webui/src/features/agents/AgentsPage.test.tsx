import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeAll, expect, it, vi } from "vitest";
import { buildMockDataset } from "@/test/mock/generator";
import { buildTaxonomyIndex, decorate } from "@/domain/metrics";
import type { LoadedDataset } from "@/api/source";
import { Providers } from "@/app/providers";
import { queryClient } from "@/app/queryClient";
import { Workbench } from "@/components/layout/Workbench";
import { useFilters } from "@/features/filters/useFilters";
import { useAnalytics } from "@/features/filters/useAnalytics";
import type * as Workbook from "@/features/export/workbook";
import { AgentsPage } from "./AgentsPage";

vi.mock("@/components/charts/EChart", () => ({
  EChart: ({ ariaLabel }: { ariaLabel: string }) => <div role="img" aria-label={ariaLabel} />,
}));
// 聚合接口落到 mock 事件上（口径的前端对照实现），理由见 `test/aggregateStub.ts`。
const stub = vi.hoisted(() => ({ events: [], groupDaily: [], tax: new Map() }) as never);
vi.mock("@/api/source", async (importOriginal) => {
  const { sourceStub } = await import("@/test/aggregateStub");
  return sourceStub(await importOriginal(), stub);
});
// 下载本身（浏览器里写文件）替换掉，只验证页面交给它的那张表。
const download = vi.hoisted(() =>
  vi.fn<(sheets: Workbook.WorkbookSheet[], view: unknown, prefix: string) => Promise<void>>(),
);
vi.mock("@/features/export/workbook", async (importOriginal) => ({
  ...(await importOriginal<typeof Workbook>()),
  downloadWorkbook: download,
}));
afterEach(cleanup);
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

const raw = buildMockDataset();
const taxIndex = buildTaxonomyIndex(raw.meta.taxonomy, raw.meta.taxonomy_version);
const dataset: LoadedDataset = { ...raw, taxIndex, source: "api", loadedAt: 0 };
Object.assign(stub, {
  events: decorate(raw.events, taxIndex),
  groupDaily: raw.groupDaily,
  tax: taxIndex,
  rooms: raw.meta.rooms,
});

function Harness() {
  const api = useFilters();
  return <AgentsPage analytics={useAnalytics(dataset, api.filters)} api={api} />;
}

it("exports_every_filtered_agent_not_only_the_current_page", async () => {
  const user = userEvent.setup();
  // mock 只有 8 个客服，页大小 5 才翻得出第二页。
  const view = render(
    <MemoryRouter initialEntries={["/agents?size=5"]}>
      <Providers>
        <Workbench>
          <Harness />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  await waitFor(() => expect(view.container.querySelector(".c2e-page")).toBeNull());
  const total = Number(/共 (\d+) 人/.exec(screen.getByText(/共 \d+ 人/).textContent)![1]);
  expect(total).toBeGreaterThan(5);
  expect(view.container.querySelectorAll(".ag-table tbody tr.ant-table-row")).toHaveLength(5);

  await user.click(screen.getByRole("button", { name: /导出 Excel/ }));
  await waitFor(() => expect(download).toHaveBeenCalledTimes(1));
  const [sheets] = download.mock.calls[0]!;
  expect(sheets.map((s) => s.name)).toEqual(["客服效能", "客服×一级分类", "客服×二级分类"]);
  // 每个客服一行 —— 不受当前页大小限制；两张宽表与第一张逐行对齐。
  for (const sheet of sheets) expect(sheet.rows).toHaveLength(total);
  // 宽表的数据是点了导出才取的，带着页面当前的筛选：每行各分类列 + 未归类 + 打标未完成
  // 等于参与事件数（对不上导出会直接失败，走不到这里）。
  for (const sheet of sheets.slice(1)) {
    const from = sheet.columns.findIndex((c) => c.header === "参与事件数");
    for (const r of sheet.rows) {
      expect((r.slice(from + 1) as number[]).reduce((a, b) => a + b, 0)).toBe(r[from]);
    }
  }
});

it("exports_only_the_selected_agent_even_though_co_participants_come_back", async () => {
  const user = userEvent.setup();
  // 选一个与别人共同参与过事件的客服：接口会把那些同事一并带回，表格只留他一个。
  const shared = raw.events.find((e) => new Set(e.agents).size > 1)!;
  const agent = shared.agents[0]!;
  const view = render(
    <MemoryRouter initialEntries={[`/agents?agent=${agent}`]}>
      <Providers>
        <Workbench>
          <Harness />
        </Workbench>
      </Providers>
    </MemoryRouter>,
  );
  await waitFor(() => expect(view.container.querySelector(".c2e-page")).toBeNull());

  await user.click(screen.getByRole("button", { name: /导出 Excel/ }));
  await waitFor(() => expect(download).toHaveBeenCalledTimes(1));
  const [sheets] = download.mock.calls[0]!;
  expect(sheets.map((s) => s.name)).toEqual(["客服效能", "客服×一级分类", "客服×二级分类"]);
  for (const sheet of sheets) expect(sheet.rows).toHaveLength(1);
});
