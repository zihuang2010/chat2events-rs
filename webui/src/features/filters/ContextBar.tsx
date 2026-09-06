/**
 * 下钻路径与已选条件。**每一项都能单独摘掉**，回上一级时保留其余条件，
 * 这样从「概览的未回复」点进明细再退回去，不会把日期和群一起丢掉。
 */

import { Breadcrumb, Tag } from "antd";
import { Link, useLocation } from "react-router-dom";
import { EVENT_STATUS, NAV, SLA_OPTIONS, DEFAULT_SLA_SEC } from "@/domain/definitions";
import type { Analytics } from "./useAnalytics";
import type { FiltersApi } from "./useFilters";

interface Chip {
  key: string;
  label: string;
  value: string;
  clear: () => void;
}

export function ContextBar({
  analytics,
  api,
  part = "all",
}: {
  analytics: Analytics;
  api: FiltersApi;
  part?: "all" | "breadcrumb" | "filters";
}) {
  const { pathname, search } = useLocation();
  const { filters, patch } = api;
  const meta = analytics.dataset.meta;
  const current = NAV.find((n) => n.path === pathname) ?? NAV[0];

  const trail: { path: string; label: string }[] = [];
  if (current.key !== "overview") trail.push({ path: "/overview", label: "整体概览" });
  if (current.key === "detail") {
    if (filters.room) trail.push({ path: "/rooms", label: "群聊洞察" });
    else if (filters.agent) trail.push({ path: "/agents", label: "客服效能" });
    else if (filters.level1 ?? filters.level2) trail.push({ path: "/events", label: "事件洞察" });
  }

  const chips: Chip[] = [];
  const first = meta.days[0];
  const last = meta.days[meta.days.length - 1];
  if (analytics.days.length !== meta.days.length) {
    const a = analytics.days[0];
    const b = analytics.days[analytics.days.length - 1];
    chips.push({
      key: "date",
      label: "日期",
      value: analytics.days.length === 0 ? "空区间" : a === b ? (a as string) : `${a} 至 ${b}`,
      clear: () => patch({ from: first ?? null, to: last ?? null }),
    });
  }
  if (filters.room)
    chips.push({
      key: "room",
      label: "群",
      value: analytics.roomLabel(filters.room),
      clear: () => patch({ room: null }),
    });
  if (filters.agent)
    chips.push({
      key: "agent",
      label: "客服",
      value: analytics.agentLabel(filters.agent),
      clear: () => patch({ agent: null, focusAgent: null }),
    });
  if (filters.level1)
    chips.push({
      key: "l1",
      label: "一级",
      value: filters.level1,
      clear: () => patch({ level1: null }),
    });
  if (filters.level2)
    chips.push({
      key: "l2",
      label: "二级",
      value: analytics.typeLabel(filters.level2),
      clear: () => patch({ level2: null }),
    });
  if (filters.status)
    chips.push({
      key: "status",
      label: "状态",
      value: EVENT_STATUS[filters.status],
      clear: () => patch({ status: null }),
    });
  if (filters.overdueOnly !== null)
    chips.push({
      key: "overdue",
      label: "超时",
      value: filters.overdueOnly ? "仅超时" : "仅未超时",
      clear: () => patch({ overdueOnly: null }),
    });
  if (filters.query.trim())
    chips.push({
      key: "q",
      label: "搜索",
      value: filters.query.trim(),
      clear: () => patch({ query: "" }),
    });
  if (filters.slaSec !== DEFAULT_SLA_SEC)
    chips.push({
      key: "sla",
      label: "阈值",
      value: SLA_OPTIONS.find((o) => o.value === filters.slaSec)?.label ?? `${filters.slaSec} 秒`,
      clear: () => patch({ slaSec: DEFAULT_SLA_SEC }),
    });

  if (part === "filters" && chips.length === 0) return null;

  return (
    <div className="c2e-contextbar">
      {part !== "filters" && (
        <Breadcrumb
          items={[
            ...trail.map((t) => ({
              title: <Link to={{ pathname: t.path, search }}>{t.label}</Link>,
            })),
            {
              title: (
                <span style={{ color: "var(--c2e-ink)", fontWeight: 600 }}>{current.label}</span>
              ),
            },
          ]}
        />
      )}
      {part !== "breadcrumb" &&
        chips.map((c) => (
          <Tag
            key={c.key}
            closable
            onClose={(e) => {
              e.preventDefault();
              c.clear();
            }}
            color="processing"
            style={{ marginInlineEnd: 0, maxWidth: 260 }}
          >
            <span style={{ color: "var(--c2e-ink-muted)" }}>{c.label} </span>
            <b>{c.value}</b>
          </Tag>
        ))}
    </div>
  );
}
