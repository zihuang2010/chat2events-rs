/**
 * 只读 JSON 接口客户端。**全部 GET**：看板不写任何表、不调模型、不参与跑批。
 *
 * 每个响应都过一遍 zod。理由不是洁癖：这块看板给上级看，一个把 NULL 当 0 的字段
 * 会产出「偏小但看起来正常」的数字，而那种错没人会发现。宁可在边界上显式失败。
 */

import { z, type ZodType } from "zod";
import { validDate } from "@/lib/format";
import { RESPONSE_BIN_EDGES } from "@/domain/definitions";
import {
  rawDatasetSchema,
  eventSchema,
  messageListSchema,
  summarySchema,
  roomAggListSchema,
  agentAggListSchema,
  categoryAggListSchema,
  eventsPageSchema,
  type SummaryRow,
  type RoomAgg,
  type AgentAgg,
  type CategoryAgg,
  type EventRow,
  type EventsPage,
  type GroupDailyRow,
  type MessageRow,
  type Meta,
} from "@/domain/schemas";

export const API_BASE = "/api";
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

/** 页面上下文：meta ＋ 群日记录。**不含事件明细**（理由见 `rawDatasetSchema`）。 */
export interface RawDataset {
  meta: Meta;
  groupDaily: GroupDailyRow[];
}

export interface DatasetWindow {
  from?: string | null;
  to?: string | null;
}

/**
 * 首屏唯一的一趟，**同时也是探活**：它成功才认为真接口可用。
 *
 * ⚠️ **窗口不在这边算。** 默认最近七天、夹到可用范围里，一直都是后端
 * `web::params::Period::bounds` 的活；此前这里抄了一遍，代价是必须先打一个
 * `/api/meta` 把 `days` 拿回来 —— 那一趟的产物除了 `days` 全被丢掉。
 * 这里只滤掉 URL 里的非法日期（当作没给），其余交给后端。
 *
 * 副作用是好的：不给日期时 URI 就是裸的 `/dataset`，跨天也不变，
 * 响应缓存的键（完整 URI）因此更稳。
 */
export async function fetchDataset(period: DatasetWindow = {}): Promise<RawDataset> {
  // 走 `params` 而不是自己拼对象 —— 它守着「空值不进 URL」那条，
  // 绕过去就会拼出 `?from=null`，正是那行注释警告的缓存键分叉。
  return get("/dataset", rawDatasetSchema, {
    ...params({ from: validDate(period.from), to: validDate(period.to) }),
  });
}

/**
 * 聚合接口共用的一组筛选参数 —— **和后端 `web::query::Filters` 一一对应**。
 *
 * ⚠️ **`level1`（父类）不在这里**：词表在前端手上，父类展开成 `types=a,b,c` 再传，
 * 免得后端每条 SQL 都 join 一次词表、多出一处能和前端打架的口径。
 *
 * ⚠️ **`q` 只匹配事件摘要**。搜索框还会命中群名 / 客服名 / 类型名，那些是前端的
 * 标签映射 —— 由调用方先解析成 id 集合，走 `room` / `agent` / `types` 传。
 */
export interface QueryFilters {
  from?: string | null;
  to?: string | null;
  room?: string | null;
  /** 参与过（`agents[]` 里有他） */
  agent?: string | null;
  /** **首响归属**给他。与 `agent` 是两个口径，可以同时给 */
  responder?: string | null;
  types?: readonly string[];
  /** **排除**这些 type_id。「未归类」只能这么表达 —— 词表外的编码列不出名单 */
  typesExclude?: readonly string[];
  status?: string | null;
  overdueOnly?: boolean | null;
  q?: string | null;
  slaSec?: number;
}

/** 只保留真正给了值的键 —— 空值进 URL 会让缓存键分叉，同一份数据取两遍。 */
function params(f: QueryFilters): Record<string, string> {
  const out: Record<string, string> = {};
  if (f.from) out.from = f.from;
  if (f.to) out.to = f.to;
  if (f.room) out.room = f.room;
  if (f.agent) out.agent = f.agent;
  if (f.responder) out.responder = f.responder;
  if (f.types?.length) out.types = f.types.join(",");
  if (f.typesExclude?.length) out.types_exclude = f.typesExclude.join(",");
  if (f.status) out.status = f.status;
  if (f.overdueOnly !== null && f.overdueOnly !== undefined)
    out.overdue_only = String(f.overdueOnly);
  if (f.q) out.q = f.q;
  if (f.slaSec !== undefined) out.sla_sec = String(f.slaSec);
  return out;
}

/**
 * 概览 KPI ＋ 按天 / 按小时序列 ＋ 首响直方图。数据库算完只送数字，
 * **行数不随窗口变大**。
 *
 * 直方图的桶边界随请求发上去（`RESPONSE_BIN_EDGES`）—— 后端不自带一份，
 * 自带的那份会在这里改了之后继续沉默地按老边界分。
 */
export const fetchSummary = (f: QueryFilters): Promise<SummaryRow> =>
  get("/summary", summarySchema, {
    ...params(f),
    buckets: RESPONSE_BIN_EDGES.join(","),
  });

/**
 * 分类汇总。不给 `groups` 就按 `type_id` 分组（二级）；
 * 给了就按分组下标分（一级），**下标顺序必须与调用方自己的父类顺序一致**。
 *
 * ⚠️ 一级的分位数只能这样拿 —— 分位数不可加，把几个二级的 p50 合起来是错的。
 */
export const fetchCategories = (
  f: QueryFilters,
  groups?: readonly (readonly string[])[],
): Promise<CategoryAgg[]> =>
  get("/categories", categoryAggListSchema, {
    ...params(f),
    ...(groups?.length ? { groups: groups.map((g) => g.join("|")).join(",") } : {}),
  });

/**
 * 按群一行，最多「群数」行。`groups` 只决定 `topGroups` 按什么分类聚 ——
 * 和 `/api/categories` 是同一套父类分组，后端一样不 join 词表。
 */
export const fetchRoomAggs = (
  f: QueryFilters,
  groups?: readonly (readonly string[])[],
): Promise<RoomAgg[]> =>
  get("/rooms", roomAggListSchema, {
    ...params(f),
    ...(groups?.length ? { groups: groups.map((g) => g.join("|")).join(",") } : {}),
  });

/** 按客服一行，最多「客服数」行。 */
export const fetchAgentAggs = (f: QueryFilters): Promise<AgentAgg[]> =>
  get("/agents", agentAggListSchema, params(f));

/** 明细表的排序。`sort` 的取值见 `EVENT_SORTS`；不给就按归属日。 */
export interface EventSorting {
  sort?: string | null;
  dir?: "asc" | "desc" | null;
}

/**
 * 事件明细的一页 —— **行、总数、页数、截断标志一起回来**。
 *
 * 服务端延迟关联翻页，`page` 1~200、`pageSize` 1~100，越界 400；
 * 页码在护栏之内但越过实际页数时返回**空数组**，不是错误。
 *
 * **排序也在服务端**：白名单之外的键那边直接 400，不静默退回默认序。
 */
export const fetchEventsPage = (
  f: QueryFilters,
  page: number,
  pageSize: number,
  sorting: EventSorting = {},
): Promise<EventsPage> =>
  get("/events", eventsPageSchema, {
    ...params(f),
    page: String(page),
    page_size: String(pageSize),
    ...(sorting.sort ? { sort: sorting.sort, dir: sorting.dir ?? "asc" } : {}),
  });

/** 消息原文。抽取时落在 b_merchant_group_event.source_messages，与事件同寿；410 = 该事件早于原文留存。 */
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
