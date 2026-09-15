import { afterEach, describe, expect, it, vi } from "vitest";
import { fetchEvent, fetchMessages, probeMeta } from "./client";
import { buildMockDataset } from "@/test/mock/generator";

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("只读接口边界", () => {
  it("补读事件不能与当前页面词表混版", async () => {
    const dataset = buildMockDataset();
    const event = dataset.events[0]!;
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(Response.json(event))),
    );
    expect((await fetchEvent(event.id, dataset.meta.taxonomy_version)).id).toBe(event.id);
    await expect(fetchEvent(event.id, "v-next")).rejects.toMatchObject({
      kind: "contract",
      path: `/event/${event.id}`,
    });
  });
  it("保留后端受控的原文缺失原因", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve(
          Response.json({ error: "该事件早于原文留存，取不到原文" }, { status: 410 }),
        ),
      ),
    );
    await expect(fetchMessages(7)).rejects.toMatchObject({
      status: 410,
      detail: "该事件早于原文留存，取不到原文",
    });
  });
  it("成功响应校验后返回，不发送写请求", async () => {
    const fetch = vi.fn(() => Promise.resolve(Response.json([])));
    vi.stubGlobal("fetch", fetch);
    expect(await fetchMessages(7)).toEqual([]);
    const [url, options] = fetch.mock.calls[0] as unknown as [URL, RequestInit];
    expect(url.pathname).toBe("/api/event/7/messages");
    expect(options.method ?? "GET").toBe("GET");
  });

  it.each([
    [() => new Response("broken", { headers: { "content-type": "application/json" } }), "contract"],
    [() => new Response("<html/>", { headers: { "content-type": "text/html" } }), "contract"],
    [() => Response.json({ messages: [] }), "contract"],
    [() => new Response(null, { status: 403 }), "http"],
  ])("显式报告错误响应 %#", async (response, kind) => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(response())),
    );
    await expect(fetchMessages(7)).rejects.toMatchObject({ kind, path: "/event/7/messages" });
  });

  it("收到响应头后正文一直不结束，也会在探活期限内超时", async () => {
    vi.useFakeTimers();
    vi.stubGlobal(
      "fetch",
      vi.fn((_url: URL, options: RequestInit) =>
        Promise.resolve({
          ok: true,
          status: 200,
          headers: new Headers({ "content-type": "application/json" }),
          json: () =>
            new Promise((_resolve, reject) => {
              options.signal?.addEventListener("abort", () =>
                reject(new DOMException("aborted", "AbortError")),
              );
            }),
        }),
      ),
    );
    const result = expect(probeMeta()).rejects.toMatchObject({ kind: "timeout", path: "/meta" });
    await vi.advanceTimersByTimeAsync(2500);
    await result;
    expect(vi.getTimerCount()).toBe(0);
  });
});
