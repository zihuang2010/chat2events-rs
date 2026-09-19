import { afterEach, describe, expect, it, vi } from "vitest";
import { buildMockDataset } from "@/test/mock/generator";
import { loadDataset } from "./source";
import type { RawDataset } from "./client";

/**
 * ⚠️ **首屏只有一趟 `/api/dataset`。** 曾经是「先探 `/api/meta` 拿 days 算窗口，再拉
 * dataset」，而 dataset 的响应体本来就自带同一份 meta —— 那一趟的产物全被丢掉。
 * 2026-09-19 连同那个路由一起删了，所以这个文件里任何对 `/api/meta` 的期待都不该回来。
 */
function stubDataset(raw: RawDataset) {
  const responses: Record<string, unknown> = {
    "/api/dataset": {
      ...raw,
      groupDaily: raw.groupDaily.map((r) => ({ ...r, freshness: "known" })),
    },
  };
  const fetch = vi.fn((url: URL) =>
    Promise.resolve(
      url.pathname in responses
        ? Response.json(responses[url.pathname])
        : new Response(null, { status: 503 }),
    ),
  );
  vi.stubGlobal("fetch", fetch);
  return fetch;
}

afterEach(() => vi.unstubAllGlobals());

describe("数据源仲裁", () => {
  // 窗口的默认与夹取都在后端（`web::params::Period::bounds`，那边有自己的测试）。
  // 这边只管两件事：不给就一个日期参数都不发；给了合法的就原样上去。
  it("默认不带日期参数，显式日期原样进入后端过滤", async () => {
    const raw = buildMockDataset();
    const fetch = stubDataset(raw);
    await loadDataset();
    let url = fetch.mock.calls.find(([url]) => url.pathname === "/api/dataset")![0];
    expect(url.searchParams.get("from")).toBeNull();
    expect(url.searchParams.get("to")).toBeNull();
    fetch.mockClear();
    await loadDataset({ from: "2026-08-01", to: "2026-08-03" });
    url = fetch.mock.calls.find(([url]) => url.pathname === "/api/dataset")![0];
    expect(url.searchParams.get("from")).toBe("2026-08-01");
    expect(url.searchParams.get("to")).toBe("2026-08-03");
  });

  // 地址栏是用户能手改的。原样透传会把手抖变成一个 400 错误页，所以在这边滤掉。
  it("URL 上的非法日期按未指定处理，不透传给后端", async () => {
    const fetch = stubDataset(buildMockDataset());
    await loadDataset({ from: "2026-02-30", to: "八月一日" });
    const url = fetch.mock.calls.find(([url]) => url.pathname === "/api/dataset")![0];
    expect(url.searchParams.get("from")).toBeNull();
    expect(url.searchParams.get("to")).toBeNull();
  });

  it("loads_without_unused_agent_or_failure_endpoints", async () => {
    const raw = buildMockDataset();
    const fetch = stubDataset(raw);
    const dataset = await loadDataset();
    expect(dataset.source).toBe("api");
    // ⚠️ **上下文里不再有事件明细** —— 指标走聚合接口，明细走 `/api/events` 翻页。
    expect(dataset).not.toHaveProperty("events");
    expect(dataset.groupDaily).toHaveLength(raw.groupDaily.length);
    // 一趟，且只有这一趟。多出任何一个路径都说明「先探一次」又回来了。
    expect(fetch.mock.calls.map(([url]) => url.pathname)).toEqual(["/api/dataset"]);
    expect(dataset).not.toHaveProperty("agentDaily");
    expect(dataset).not.toHaveProperty("failures");
  });

  // ⚠️ **词表混版守卫搬到后端了**：`/api/dataset` 那条 `EXISTS` 不一致直接 409，
  // 前端这边再查一遍已经不可能（明细不在这个响应里，而且是一页一页翻的）。
  // 契约测试因此改成「后端拒绝时前端不回落模拟数据」。
  it("mixed_taxonomy_conflict_is_reported_not_masked_by_mock", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve(
          new Response(JSON.stringify({ error: "事件与当前词表版本不一致，请完成重打标" }), {
            status: 409,
            headers: { "content-type": "application/json" },
          }),
        ),
      ),
    );
    await expect(loadDataset()).rejects.toMatchObject({ kind: "http", status: 409 });
  });

  it("keeps_v0_system_state_distinct_from_unmatched_categories", async () => {
    const raw = buildMockDataset();
    raw.meta = { ...raw.meta, taxonomy_version: "v0", taxonomy: [] };
    raw.events = raw.events.slice(0, 1).map((event) => ({
      ...event,
      taxonomy_version: "v0",
      event_type: "__untyped__",
    }));
    stubDataset(raw);
    const dataset = await loadDataset();
    expect(dataset.taxIndex.get("__untyped__")?.name).toBe("未建词表");
    expect(dataset.taxIndex.get("__untyped__")?.description).toContain("尚未建立词表");
  });

  it("reports_network_errors_without_demo_fallback", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.reject(new TypeError("offline"))),
    );
    await expect(loadDataset()).rejects.toMatchObject({ kind: "network" });
  });

  it.each([401, 403, 404, 500, 503])(
    "reports_probe_http_%i_without_demo_fallback",
    async (status) => {
      vi.stubGlobal(
        "fetch",
        vi.fn(() => Promise.resolve(new Response(null, { status }))),
      );
      await expect(loadDataset()).rejects.toMatchObject({ kind: "http", status });
    },
  );

  it("契约错误不回落模拟", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(Response.json({}))),
    );
    await expect(loadDataset()).rejects.toMatchObject({ kind: "contract" });
  });
});
