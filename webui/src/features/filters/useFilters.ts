/**
 * 筛选状态。**全部编码进 URL**：刷新不丢、能收藏、能直接把链接发给别人。
 * 这对给上级看的页面不是锦上添花 —— 「你看的是哪个口径」必须能被复现。
 *
 * 只有一份状态源（URLSearchParams），组件里没有第二份 useState 副本。
 */

import { useCallback, useMemo } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { DEFAULT_SLA_SEC, STATUS_FILTERS, type StatusFilter } from "@/domain/definitions";
import { addDays } from "@/lib/format";

export interface Filters {
  from: string | null;
  to: string | null;
  room: string | null;
  agent: string | null;
  level1: string | null;
  level2: string | null;
  status: StatusFilter | null;
  overdueOnly: boolean | null;
  query: string;
  slaSec: number;
  page: number;
  pageSize: number;
  /** 群聊分析的排行口径 */
  rank: string;
  /** 打开的事件详情抽屉 */
  drawer: number | null;
  /** 客服表现里展开的那个人 */
  focusAgent: string | null;
}

export type FilterPatch = Partial<Filters>;

const KEYS = {
  from: "from",
  to: "to",
  room: "room",
  agent: "agent",
  level1: "l1",
  level2: "l2",
  status: "status",
  overdueOnly: "overdue",
  query: "q",
  slaSec: "sla",
  page: "page",
  pageSize: "size",
  rank: "rank",
  drawer: "drawer",
  focusAgent: "focus",
} as const satisfies Record<keyof Filters, string>;

const DEFAULTS = { slaSec: DEFAULT_SLA_SEC, page: 1, pageSize: 20, rank: "events" } as const;

function readInt(v: string | null, fallback: number): number {
  const n = Number(v);
  return Number.isSafeInteger(n) && n > 0 ? n : fallback;
}

function readDate(value: string | null): string | null {
  return value && /^\d{4}-\d{2}-\d{2}$/.test(value) && addDays(value, 0) === value ? value : null;
}

export function parseFilters(sp: URLSearchParams): Filters {
  const status = sp.get(KEYS.status);
  const overdue = sp.get(KEYS.overdueOnly);
  const drawer = sp.get(KEYS.drawer);
  const from = readDate(sp.get(KEYS.from));
  const to = readDate(sp.get(KEYS.to));
  return {
    from,
    to: from && to && to < from ? from : to,
    room: sp.get(KEYS.room),
    agent: sp.get(KEYS.agent),
    level1: sp.get(KEYS.level1),
    level2: sp.get(KEYS.level2),
    status: STATUS_FILTERS.includes(status as StatusFilter) ? (status as StatusFilter) : null,
    overdueOnly: overdue === "1" ? true : overdue === "0" ? false : null,
    query: sp.get(KEYS.query) ?? "",
    slaSec: readInt(sp.get(KEYS.slaSec), DEFAULTS.slaSec),
    page: readInt(sp.get(KEYS.page), DEFAULTS.page),
    pageSize: readInt(sp.get(KEYS.pageSize), DEFAULTS.pageSize),
    rank: sp.get(KEYS.rank) ?? DEFAULTS.rank,
    drawer: drawer === null ? null : readInt(drawer, 0) || null,
    focusAgent: sp.get(KEYS.focusAgent),
  };
}

/** 把补丁写回查询串。默认值一律不落到 URL 上，链接才不会长得没法看。 */
export function applyPatch(sp: URLSearchParams, patch: FilterPatch): URLSearchParams {
  const next = new URLSearchParams(sp);
  // 任何筛选条件变化都把分页复位；只有显式给了 page 才保留
  if (!("page" in patch)) next.delete(KEYS.page);

  const entries = Object.entries(patch) as [
    keyof Filters,
    string | number | boolean | null | undefined,
  ][];
  for (const [field, value] of entries) {
    const key = KEYS[field];
    if (value === null || value === undefined || value === "") {
      next.delete(key);
      continue;
    }
    if (typeof value === "boolean") {
      next.set(key, value ? "1" : "0");
      continue;
    }
    const isDefault =
      (field === "slaSec" && value === DEFAULTS.slaSec) ||
      (field === "page" && value === DEFAULTS.page) ||
      (field === "pageSize" && value === DEFAULTS.pageSize) ||
      (field === "rank" && value === DEFAULTS.rank);
    if (isDefault) next.delete(key);
    else next.set(key, String(value));
  }
  return next;
}

export interface FiltersApi {
  filters: Filters;
  /** 只改筛选条件，留在当前视图 */
  patch: (p: FilterPatch, opts?: { replace?: boolean }) => void;
  /** 换视图并带上筛选条件：下钻用这个，上下文不丢 */
  go: (pathname: string, p?: FilterPatch) => void;
  reset: () => void;
  /** 生成「在当前筛选上改几项」的目标地址，给 <Link> 用 */
  hrefWith: (p: FilterPatch, pathname?: string) => string;
}

export function useFilters(): FiltersApi {
  const [searchParams, setSearchParams] = useSearchParams();
  const navigate = useNavigate();
  const filters = useMemo(() => parseFilters(searchParams), [searchParams]);

  const patch = useCallback(
    (p: FilterPatch, opts?: { replace?: boolean }) => {
      setSearchParams((prev) => applyPatch(prev, p), { replace: opts?.replace ?? false });
    },
    [setSearchParams],
  );

  const reset = useCallback(() => {
    setSearchParams(
      (previous) => {
        const next = new URLSearchParams();
        const source = previous.get("source");
        if (source === "api" || source === "mock") next.set("source", source);
        return next;
      },
      { replace: false },
    );
  }, [setSearchParams]);

  const hrefWith = useCallback(
    (p: FilterPatch, pathname?: string) => {
      const qs = applyPatch(searchParams, p).toString();
      const base = pathname ?? "";
      return qs ? `${base}?${qs}` : base || "?";
    },
    [searchParams],
  );

  const go = useCallback(
    (pathname: string, p: FilterPatch = {}) => {
      const qs = applyPatch(searchParams, p).toString();
      // react-router 的 navigate 返回 Promise，这里不需要等它
      void navigate(qs ? `${pathname}?${qs}` : pathname);
    },
    [navigate, searchParams],
  );

  return { filters, patch, go, reset, hrefWith };
}
