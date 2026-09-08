import { afterEach, describe, expect, it, vi } from "vitest";
import { buildMockDataset } from "./mock/generator";
import { loadDataset } from "./source";
import type { RawDataset } from "./client";

function stubDataset(raw: RawDataset) {
  const responses: Record<string, unknown> = {
    "/api/meta": raw.meta,
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
  it("默认只请求最近七天，显式日期进入后端过滤", async () => {
    const raw = buildMockDataset();
    raw.meta.days = ["2026-08-01", ...raw.meta.days];
    const fetch = stubDataset(raw);
    await loadDataset("api");
    let url = fetch.mock.calls.find(([url]) => url.pathname === "/api/dataset")![0];
    expect(url.searchParams.get("from")).toBe("2026-08-25");
    expect(url.searchParams.get("to")).toBe("2026-08-31");
    fetch.mockClear();
    await loadDataset("api", { from: "2026-08-01", to: "2026-08-03" });
    url = fetch.mock.calls.find(([url]) => url.pathname === "/api/dataset")![0];
    expect(url.searchParams.get("from")).toBe("2026-08-01");
    expect(url.searchParams.get("to")).toBe("2026-08-03");
  });
  it("loads_without_unused_agent_or_failure_endpoints", async () => {
    const raw = buildMockDataset();
    const fetch = stubDataset(raw);
    const dataset = await loadDataset("api");
    expect(dataset.source).toBe("api");
    expect(dataset.events).toHaveLength(raw.events.length);
    expect(fetch.mock.calls.map(([url]) => url.pathname).sort()).toEqual([
      "/api/dataset",
      "/api/meta",
    ]);
    expect(dataset).not.toHaveProperty("agentDaily");
    expect(dataset).not.toHaveProperty("failures");
  });

  it.each([false, true])("rejects_taxonomy_mismatch_without_mock_fallback_%s", async (mixed) => {
    const raw = buildMockDataset();
    raw.events = raw.events.slice(0, 2).map((event, index) => ({
      ...event,
      taxonomy_version: mixed && index === 0 ? raw.meta.taxonomy_version : "v0",
    }));
    stubDataset(raw);
    await expect(loadDataset()).rejects.toMatchObject({ kind: "contract", path: "/events" });
  });

  it("keeps_v0_system_state_distinct_from_unmatched_categories", async () => {
    const raw = buildMockDataset();
    raw.meta = { ...raw.meta, taxonomy_version: "v0", taxonomy: [] };
    raw.events = raw.events.slice(0, 1).map((event) => ({
      ...event,
      taxonomy_version: "v0",
      event_type: "__untyped__",
      event_types: ["__untyped__"],
    }));
    stubDataset(raw);
    const dataset = await loadDataset("api");
    expect(dataset.events[0]?.level2).toBe("未建词表");
    expect(dataset.taxIndex.get("__untyped__")?.description).toContain("尚未建立词表");
  });

  it("强制模拟模式不访问接口", async () => {
    const fetch = vi.fn();
    vi.stubGlobal("fetch", fetch);
    expect((await loadDataset("mock")).source).toBe("mock");
    expect(fetch).not.toHaveBeenCalled();
  });

  it("默认模式仅在探活不可达时回落，强制真实源则报错", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.reject(new TypeError("offline"))),
    );
    const dataset = await loadDataset();
    expect(dataset.source).toBe("mock");
    expect(dataset.fallbackReason).toContain("网络不可达");
    await expect(loadDataset("api")).rejects.toMatchObject({ kind: "network" });
  });

  it.each([401, 403])("探活返回 %i 不回落模拟", async (status) => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(new Response(null, { status }))),
    );
    await expect(loadDataset()).rejects.toMatchObject({ kind: "http", status });
  });

  it("探活契约错误不回落模拟", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(Response.json({}))),
    );
    await expect(loadDataset()).rejects.toMatchObject({ kind: "contract" });
  });

  it("探活成功后任一数据请求失败，整份真实数据显式报错", async () => {
    const raw = buildMockDataset();
    vi.stubGlobal(
      "fetch",
      vi.fn((url: URL) =>
        Promise.resolve(
          url.pathname === "/api/meta"
            ? Response.json(raw.meta)
            : new Response(null, { status: 503 }),
        ),
      ),
    );
    await expect(loadDataset()).rejects.toMatchObject({ kind: "http", status: 503 });
  });
});
