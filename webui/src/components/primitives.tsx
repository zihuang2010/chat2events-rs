/**
 * 展示原语。这些看起来琐碎，但每一个都对应一条不能破的规矩：
 *   NullValue  NULL 必须显示成长横线，绝不显示 0
 *   DataGap    库里没有来源的能力必须显式标注，绝不编造
 */

import { Tag, Tooltip } from "antd";
import { formatDuration, formatPercent } from "@/lib/format";
import type { EventStatusKind } from "@/domain/metrics";

export function NullValue({ reason = "NULL：没算出来，不是 0" }: { reason?: string | undefined }) {
  return (
    <Tooltip title={reason}>
      <span className="c2e-null">—</span>
    </Tooltip>
  );
}

export function DataGap({
  label = "待补数据",
  detail,
}: {
  label?: string | undefined;
  detail?: string | undefined;
}) {
  const chip = <span className="c2e-gap">{label}</span>;
  return detail ? <Tooltip title={detail}>{chip}</Tooltip> : chip;
}

/** 数字或 NULL。**这是全站唯一允许把数字渲染出来的地方之一**，保证 0 与 NULL 不混。 */
export function NumberOrNull({ value }: { value: number | null }) {
  return value === null ? <NullValue /> : <>{value.toLocaleString("zh-CN")}</>;
}

export function DurationOrNull({ value }: { value: number | null }) {
  const text = formatDuration(value);
  return text === null ? <NullValue /> : <>{text}</>;
}

export function PercentOrNull({
  value,
  digits = 1,
}: {
  value: number | null;
  digits?: number | undefined;
}) {
  const text = formatPercent(value, digits);
  return text === null ? <NullValue /> : <>{text}</>;
}

const STATUS_STYLE: Record<EventStatusKind, { color: string; label: string; tip: string }> = {
  replied: { color: "success", label: "已回复", tip: "商家发起，且有平台客服的首次有效回复" },
  overdue: { color: "warning", label: "已回复 · 超时", tip: "有回复，但首响超过了当前阈值" },
  unreplied: {
    color: "error",
    label: "无响应",
    tip: "商家发起但至今没有任何 INTERNAL 回复，不计入分位数",
  },
  push: { color: "default", label: "平台发起", tip: "平台工单推送，首响恒 0 秒，不进任何首响指标" },
};

export function StatusTag({ status }: { status: EventStatusKind }) {
  const s = STATUS_STYLE[status];
  return (
    <Tooltip title={s.tip}>
      <Tag color={s.color} style={{ marginInlineEnd: 0 }}>
        {s.label}
      </Tag>
    </Tooltip>
  );
}
