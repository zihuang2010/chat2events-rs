/** 加载、空数据、错误三态。骨架形状贴着真实布局，不用转圈圈。 */

import { Button, Empty, Result, Skeleton, Space } from "antd";
import { ApiError } from "@/api/client";

export function PageSkeleton() {
  return (
    <div className="c2e-page">
      <Skeleton.Node active style={{ width: "100%", height: 52 }} />
      <div className="c2e-kpis">
        {Array.from({ length: 6 }, (_, i) => (
          <div className="c2e-kpi" key={i}>
            <Skeleton.Input active size="small" style={{ width: 62, height: 12 }} />
            <Skeleton.Input active size="small" style={{ width: 88, height: 26, marginTop: 6 }} />
            <Skeleton.Input active size="small" style={{ width: 110, height: 11, marginTop: 6 }} />
          </div>
        ))}
      </div>
      <div className="c2e-grid c2e-wide-left">
        <Skeleton.Node active style={{ width: "100%", height: 260 }} />
        <Skeleton.Node active style={{ width: "100%", height: 260 }} />
      </div>
      <Skeleton active paragraph={{ rows: 8 }} />
    </div>
  );
}

export function EmptyState({
  title,
  description,
  onReset,
}: {
  title: string;
  description: string;
  onReset?: (() => void) | undefined;
}) {
  return (
    <Empty
      image={Empty.PRESENTED_IMAGE_SIMPLE}
      styles={{ root: { padding: "36px 16px" } }}
      description={
        <Space orientation="vertical" size={4} style={{ maxWidth: 520 }}>
          <b style={{ color: "var(--c2e-ink)" }}>{title}</b>
          <span style={{ color: "var(--c2e-ink-muted)", lineHeight: 1.7 }}>{description}</span>
        </Space>
      }
    >
      {onReset ? (
        <Button type="primary" onClick={onReset}>
          清空筛选
        </Button>
      ) : null}
    </Empty>
  );
}

export function ErrorState({ error, onRetry }: { error: unknown; onRetry: () => void }) {
  const api = error instanceof ApiError ? error : null;
  const detail = api
    ? `${api.userMessage}${api.detail ? `。${api.detail}` : ""}`
    : error instanceof Error
      ? error.message
      : "未知错误";
  return (
    <Result
      status="warning"
      title="数据加载失败"
      subTitle={
        <span style={{ lineHeight: 1.8 }}>
          {detail}
          <br />
          看板只读，不写任何表，重试是安全的。若持续失败，先确认跑批那一轮是否已经落库。
        </span>
      }
      extra={
        <Button type="primary" onClick={onRetry}>
          重试
        </Button>
      }
    />
  );
}
