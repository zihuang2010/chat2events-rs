/**
 * 概览固定采用 D 工作台，统计最新七个自然日。
 * 清理旧链接的变体和筛选参数，保留事件详情抽屉状态。
 */
import { useEffect } from "react";
import { useSearchParams } from "react-router-dom";
import { addDays } from "@/lib/format";
import { useAnalytics, type Analytics } from "@/features/filters/useAnalytics";
import { parseFilters, type FiltersApi } from "@/features/filters/useFilters";
import { OverviewDashboard } from "./OverviewDashboard";

const KEEP_ON_OVERVIEW = new Set(["drawer"]);

export function OverviewPage(props: { analytics: Analytics; api: FiltersApi }) {
  const [searchParams, setSearchParams] = useSearchParams();
  const end = props.analytics.dataset.meta.days.at(-1);
  const days = end ? Array.from({ length: 7 }, (_, i) => addDays(end, i - 6)) : [];
  const period = { from: days[0] ?? null, to: end ?? null };
  const filters = {
    ...parseFilters(new URLSearchParams()),
    ...period,
    drawer: props.api.filters.drawer,
  };
  const analytics = useAnalytics(props.analytics.dataset, filters, days);
  const api: FiltersApi = {
    ...props.api,
    filters,
    hrefWith: (patch, pathname) =>
      props.api.hrefWith(pathname ? { ...period, ...patch } : patch, pathname),
    go: (pathname, patch) => props.api.go(pathname, { ...period, ...patch }),
  };

  useEffect(() => {
    const kept = new URLSearchParams();
    let stray = false;
    for (const [k, v] of searchParams) {
      if (KEEP_ON_OVERVIEW.has(k)) kept.set(k, v);
      else stray = true;
    }
    if (stray) setSearchParams(kept, { replace: true });
  }, [searchParams, setSearchParams]);

  return <OverviewDashboard analytics={analytics} api={api} />;
}
