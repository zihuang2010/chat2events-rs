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
import type * as ExportAgents from "./exportAgents";
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
const download = vi.hoisted(() => vi.fn<(sheet: unknown[]) => Promise<void>>());
vi.mock("./exportAgents", async (importOriginal) => ({
  ...(await importOriginal<typeof ExportAgents>()),
  downloadAgentSheet: download,
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
  // 摘要 · 空行 · 表头之后，每个客服一行 —— 不受当前页大小限制。
  expect(download.mock.calls[0]![0]).toHaveLength(3 + total);
});
