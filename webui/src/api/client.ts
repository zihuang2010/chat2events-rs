/**
 * 只读 JSON 接口客户端。**全部 GET**：看板不写任何表、不调模型、不参与跑批。
 *
 * 每个响应都过一遍 zod。理由不是洁癖：这块看板给上级看，一个把 NULL 当 0 的字段
 * 会产出「偏小但看起来正常」的数字，而那种错没人会发现。宁可在边界上显式失败。
 */

import { z, type ZodType } from "zod";
import { windowBounds } from "@/lib/format";
import {
  rawDatasetSchema,
  eventSchema,
  messageListSchema,
  metaSchema,
  type EventRow,
  type GroupDailyRow,
  type MessageRow,
  type Meta,
} from "@/domain/schemas";

export const API_BASE = "/api";
const PROBE_TIMEOUT_MS = 2500;
const READ_TIMEOUT_MS = 20000;

export type ApiErrorKind = "network" | "timeout" | "http" | "contract";

export class ApiError extends Error {
  readonly kind: ApiErrorKind;
  readonly status: number;
  readonly path: string;
  readonly detail: string | undefined;

  constructor(
    message: string,
    opts: { kind: ApiErrorKind; status?: number; path: string; detail?: string },
  ) {
    super(message);
    this.name = "ApiError";
    this.kind = opts.kind;
    this.status = opts.status ?? 0;
    this.path = opts.path;
    this.detail = opts.detail;
  }

  /** 给用户看的一句话，不暴露栈，也不隐瞒是哪一类失败。 */
  get userMessage(): string {
    switch (this.kind) {
      case "timeout":
        return `请求超时：${this.path}`;
      case "network":
        return `网络不可达：${this.path}`;
      case "http":
        return `接口返回 ${this.status}：${this.path}`;
      case "contract":
        return `接口返回的数据不符合约定：${this.path}`;
    }
  }
}

async function get<T>(
  path: string,
  schema: ZodType<T>,
  params?: Record<string, string | number | undefined>,
  timeoutMs: number = READ_TIMEOUT_MS,
): Promise<T> {
  const url = new URL(API_BASE + path, location.origin);
  for (const [k, v] of Object.entries(params ?? {})) {
    if (v !== undefined) url.searchParams.set(k, String(v));
  }

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(url, {
      signal: controller.signal,
      headers: { accept: "application/json" },
    });
    if (!res.ok) {
      let detail: string | undefined;
      if ((res.headers.get("content-type") ?? "").includes("json")) {
        const error = z.object({ error: z.string() }).safeParse(await res.json().catch(() => null));
        if (error.success) detail = error.data.error;
      }
      throw new ApiError(`HTTP ${res.status}`, {
        kind: "http",
        status: res.status,
        path,
        ...(detail === undefined ? {} : { detail }),
      });
    }
    if (!(res.headers.get("content-type") ?? "").toLowerCase().includes("json")) {
      throw new ApiError("返回的不是 JSON", {
        kind: "contract",
        status: res.status,
        path,
        detail: "检查 /api 的反向代理是否返回了静态页面",
      });
    }
    let body: unknown;
    try {
      body = await res.json();
    } catch (error) {
      if (!(error instanceof SyntaxError)) throw error;
      throw new ApiError("JSON 格式错误", { kind: "contract", status: res.status, path });
    }
    const parsed = schema.safeParse(body);
    if (!parsed.success) {
      const first = parsed.error.issues[0];
      throw new ApiError("数据不符合约定", {
        kind: "contract",
        status: res.status,
        path,
        detail: first ? `${first.path.join(".") || "(根)"}：${first.message}` : "未知字段错误",
      });
    }
    return parsed.data;
  } catch (error) {
    if (error instanceof ApiError) throw error;
    throw new ApiError(controller.signal.aborted ? "请求超时" : "网络不可达", {
      kind: controller.signal.aborted ? "timeout" : "network",
      path,
    });
  } finally {
    // fetch 收到响应头就会完成，计时器必须覆盖正文下载与 JSON 解析。
    clearTimeout(timer);
  }
}

/** 探活。只有这一个请求成功，才认为真接口可用。 */
export const probeMeta = (): Promise<Meta> => get("/meta", metaSchema, undefined, PROBE_TIMEOUT_MS);

export interface RawDataset {
  meta: Meta;
  events: EventRow[];
  groupDaily: GroupDailyRow[];
}

export interface DatasetWindow {
  from?: string | null;
  to?: string | null;
}

export async function fetchDataset(meta: Meta, period: DatasetWindow = {}): Promise<RawDataset> {
  const { from, to } = windowBounds(meta.days, period.from, period.to);
  return get("/dataset", rawDatasetSchema, { from, to });
}

/** 消息原文。真接口走摄取端口的 read_by_ids，可见范围受 raw 区保留期限制。 */
export const fetchMessages = (eventId: number): Promise<MessageRow[]> =>
  get(`/event/${encodeURIComponent(String(eventId))}/messages`, messageListSchema);

/** 深链接可能指向当前日期范围之外的事件，按行 id 单独读取。 */
export async function fetchEvent(eventId: number, taxonomyVersion: string): Promise<EventRow> {
  const path = `/event/${encodeURIComponent(String(eventId))}`;
  const event = await get(path, eventSchema);
  if (event.taxonomy_version !== null && event.taxonomy_version !== taxonomyVersion) {
    throw new ApiError("事件与当前页面词表版本不一致", {
      kind: "contract",
      path,
      detail: "词表已变化，请刷新页面后重试",
    });
  }
  return event;
}
