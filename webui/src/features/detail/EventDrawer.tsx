import { LeftOutlined, RightOutlined } from "@ant-design/icons";
import { Alert, Button, Drawer, Tabs, Tag } from "antd";
import { useEffect, useRef } from "react";
import type { DecoratedEvent } from "@/domain/schemas";
import { isMerchant, statusOf } from "@/domain/metrics";
import { DurationOrNull, StatusTag } from "@/components/primitives";
import type { Analytics } from "@/features/filters/useAnalytics";
import { WORKBENCH_THEME, cssVars } from "@/app/theme/workbench";
import { EventMessageThread } from "./EventMessageThread";
import { EventProperties, EventBasis } from "./EventEvidence";
import "./event-drawer.css";

export function EventDrawer({
  event,
  missingId,
  outsideFilter,
  analytics,
  onClose,
  position,
  onPrevious,
  onNext,
}: {
  event: DecoratedEvent | undefined;
  missingId?: number | null | undefined;
  outsideFilter?: boolean | undefined;
  analytics: Analytics;
  onClose: () => void;
  position?: string | undefined;
  onPrevious?: (() => void) | undefined;
  onNext?: (() => void) | undefined;
}) {
  const navigationButton = useRef<HTMLElement | null>(null);
  useEffect(() => {
    // 首尾按钮禁用时把焦点交给另一按钮，保持连续浏览与 Escape 可用。
    if (navigationButton.current?.matches(":disabled")) {
      navigationButton.current
        .closest(".ed-navigation-actions")
        ?.querySelector<HTMLButtonElement>("button:not(:disabled)")
        ?.focus({ preventScroll: true });
    }
    navigationButton.current = null;
  }, [event?.id]);

  return (
    <Drawer
      open={event !== undefined || missingId != null}
      onClose={onClose}
      size="min(860px, 100vw)"
      rootStyle={cssVars(WORKBENCH_THEME)}
      rootClassName="ia-event-drawer"
      closable={{ placement: "end" }}
      destroyOnHidden
      title={
        <div className="ed-title">
          <span>{event ? `事件 #${event.id}` : "事件详情"}</span>
          {analytics.dataset.source === "mock" ? <Tag color="warning">模拟数据</Tag> : null}
        </div>
      }
      footer={
        event ? (
          <div className="ed-navigation">
            <span className="ed-position">
              {position ? (
                <>
                  当前事件 <b>{position}</b>
                </>
              ) : outsideFilter ? (
                "筛选范围外的事件"
              ) : (
                `事件 #${event.id}`
              )}
            </span>
            <div className="ed-navigation-actions">
              <Button
                aria-label="上一条事件"
                title="上一条事件"
                icon={<LeftOutlined />}
                disabled={!onPrevious}
                onClick={(e) => {
                  navigationButton.current = e.currentTarget;
                  onPrevious?.();
                }}
              />
              <Button
                aria-label="下一条事件"
                title="下一条事件"
                icon={<RightOutlined />}
                disabled={!onNext}
                onClick={(e) => {
                  navigationButton.current = e.currentTarget;
                  onNext?.();
                }}
              />
            </div>
          </div>
        ) : null
      }
    >
      {event ? (
        <EventDrawerContent
          key={event.id}
          event={event}
          analytics={analytics}
          outsideFilter={outsideFilter}
        />
      ) : (
        <div className="ed-panel">
          <Alert
            type="warning"
            showIcon
            title={`找不到事件 #${missingId ?? ""}`}
            description="这个 ID 不在当前已装载的数据窗口中。请检查日期范围；事件也可能因对应群日抽取失败而未生成。"
          />
        </div>
      )}
    </Drawer>
  );
}

function EventDrawerContent({
  event,
  analytics,
  outsideFilter,
}: {
  event: DecoratedEvent;
  analytics: Analytics;
  outsideFilter: boolean | undefined;
}) {
  const status = statusOf(event, analytics.slaSec);
  return (
    <>
      <header className="ed-overview">
        <div className="ed-status-line">
          <StatusTag status={status} />
          {event.crossDay ? <Tag>跨天</Tag> : null}
          <span className="ed-category" title={`${event.level1} / ${event.level2}`}>
            {event.level1} / {event.level2}
          </span>
        </div>
        <h2 className="ed-summary" title={event.summary}>
          {event.summary}
        </h2>
        <div className="ed-context" aria-label="事件关键数据">
          <div className="ed-origin">
            <strong title={analytics.roomLabel(event.roomid)}>
              {analytics.roomLabel(event.roomid)}
            </strong>
            <time dateTime={event.first_msg_time.replace(" ", "T")}>
              {event.first_msg_time.slice(0, 16)} <span>UTC+8</span>
            </time>
          </div>
          <div className="ed-response" data-status={status}>
            <span>首响耗时</span>
            <strong>
              {isMerchant(event) ? <DurationOrNull value={event.firstReplySec} /> : "不适用"}
            </strong>
            <span
              title={
                event.first_responder ? analytics.agentLabel(event.first_responder) : undefined
              }
            >
              {isMerchant(event)
                ? event.first_responder
                  ? analytics.agentLabel(event.first_responder)
                  : "暂无客服响应"
                : "平台发起"}
            </span>
          </div>
        </div>
        {outsideFilter ? (
          <Alert className="ed-outside" type="info" showIcon title="这条事件不在当前筛选结果里" />
        ) : null}
      </header>
      <Tabs
        className="ed-tabs"
        defaultActiveKey="messages"
        animated={false}
        items={[
          {
            key: "messages",
            label: (
              <>
                消息时间线 <span className="ed-count">{event.source_msg_ids.length}</span>
              </>
            ),
            children: <EventMessageThread event={event} analytics={analytics} />,
          },
          {
            key: "properties",
            label: "事件资料",
            children: (
              <div className="ed-panel">
                <EventProperties event={event} analytics={analytics} />
              </div>
            ),
          },
          {
            key: "basis",
            label: "统计依据",
            children: (
              <div className="ed-panel">
                <EventBasis event={event} analytics={analytics} />
              </div>
            ),
          },
        ]}
      />
    </>
  );
}
