import { ArrowRightOutlined, InfoCircleOutlined } from "@ant-design/icons";
import { Tooltip } from "antd";
import type { ReactNode } from "react";
import { Link } from "react-router-dom";

export function MetricInfo({ label, text }: { label?: string; text: string }) {
  return (
    <Tooltip title={text} mouseEnterDelay={0.8} trigger={["hover", "focus"]}>
      <button type="button" className="od-info" aria-label={label ?? `口径说明：${text}`}>
        <InfoCircleOutlined aria-hidden="true" />
      </button>
    </Tooltip>
  );
}

export function Metric({
  label,
  value,
  unit,
  note,
  info,
  to,
  tone,
  unavailable = false,
}: {
  label: string;
  value: string;
  unit?: string | undefined;
  note?: ReactNode;
  info?: string | undefined;
  to?: string | undefined;
  tone?: "risk" | "warn" | "good" | undefined;
  unavailable?: boolean;
}) {
  return (
    <div className="od-metric" data-tone={unavailable ? undefined : tone}>
      <div className="od-metric-label">
        {label}
        {info ? <MetricInfo label={`${label}口径`} text={info} /> : null}
      </div>
      {to && !unavailable ? (
        <Link
          className="od-metric-value"
          to={to}
          aria-label={`${label}：${value}${unit ?? ""}，查看明细`}
        >
          {value}
          <small>{unit}</small>
          <ArrowRightOutlined className="od-metric-arrow" aria-hidden="true" />
        </Link>
      ) : (
        <div className="od-metric-value">
          {unavailable ? "—" : value}
          <small>{unit}</small>
        </div>
      )}
      <p>{unavailable ? "当前范围事件统计暂缺" : note}</p>
    </div>
  );
}
