import { afterEach, describe, expect, it, vi } from "vitest";
import { buildMockDataset } from "./mock/generator";
import { loadDataset } from "./source";

afterEach(() => vi.unstubAllGlobals());

describe("数据源仲裁", () => {
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
