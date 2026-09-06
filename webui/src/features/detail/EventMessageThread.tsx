import {
  AimOutlined,
  CustomerServiceOutlined,
  InfoCircleOutlined,
  ReloadOutlined,
  ShopOutlined,
} from "@ant-design/icons";
import { Alert, Button, Checkbox, Skeleton, Typography } from "antd";
import { Fragment, useMemo, useRef, useState } from "react";
import { useEventMessages } from "@/api/queries";
import { ApiError } from "@/api/client";
import { isMerchant } from "@/domain/metrics";
import type { DecoratedEvent } from "@/domain/schemas";
import type { Analytics } from "@/features/filters/useAnalytics";
import { shortId, weekdayOf } from "@/lib/format";

export function EventMessageThread({
  event,
  analytics,
}: {
  event: DecoratedEvent;
  analytics: Analytics;
}) {
  const messages = useEventMessages(analytics.dataset.source, event.id);
  const [showIds, setShowIds] = useState(false);
  const anchorRef = useRef<HTMLLIElement | null>(null);
  const ordered = useMemo(
    () => [...(messages.data ?? [])].sort((a, b) => a.at.localeCompare(b.at)),
    [messages.data],
  );
  const anchorIndex =
    event.first_agent_reply_time === null
      ? -1
      : ordered.findIndex(
          (m) =>
            m.at === event.first_agent_reply_time &&
            m.sender_role === "INTERNAL" &&
            (!event.first_responder || m.sender_id === event.first_responder),
        );
  const returnedIds = new Set(ordered.map((m) => m.msg_id));
  const missing = event.source_msg_ids.filter((id) => !returnedIds.has(id)).length;
  const merchant = isMerchant(event);
  return (
    <div className="ed-message-view">
      <div className="ed-message-toolbar">
        <span>
          {messages.isPending
            ? "加载原文中"
            : messages.isError
              ? "原文加载失败"
              : `${ordered.length} 条原文`}
          <span className="ed-toolbar-zone"> · UTC+8</span>
        </span>
        <div>
          <Checkbox checked={showIds} onChange={(e) => setShowIds(e.target.checked)}>
            消息标识
          </Checkbox>
          <Button
            icon={<AimOutlined />}
            title="定位首响"
            aria-label="定位首响"
            disabled={!merchant || anchorIndex < 0 || messages.isError || messages.isPending}
            onClick={() => {
              anchorRef.current?.scrollIntoView({ block: "center", behavior: "instant" });
              anchorRef.current?.focus({ preventScroll: true });
            }}
          />
        </div>
      </div>
      <div className="ed-panel ed-message-scroll" aria-label="消息原文">
        {messages.isPending ? (
          <div className="ed-loading" role="status" aria-label="正在加载消息原文">
            {[0, 1, 2].map((key) => (
              <Skeleton key={key} avatar active paragraph={{ rows: 2 }} />
            ))}
          </div>
        ) : messages.isError ? (
          <Alert
            type="warning"
            showIcon
            title="取不到消息原文"
            description={
              <>
                <p>
                  {messages.error instanceof ApiError
                    ? messages.error.userMessage
                    : messages.error.message}
                </p>
                {messages.error instanceof ApiError && messages.error.detail ? (
                  <p>{messages.error.detail}</p>
                ) : null}
                <p className="ed-field-note">原文可见范围受保留期限制，事件统计记录仍保留。</p>
                <Button
                  className="ed-retry"
                  icon={<ReloadOutlined />}
                  loading={messages.isFetching}
                  onClick={() => void messages.refetch()}
                  aria-label="重试消息原文"
                >
                  重试
                </Button>
              </>
            }
          />
        ) : ordered.length === 0 ? (
          <div className="ed-empty" role="status">
            <InfoCircleOutlined aria-hidden="true" />
            <h3>暂无消息原文</h3>
            <p>接口未返回来源消息，无法核实首响锚点。事件统计记录仍保留。</p>
          </div>
        ) : (
          <>
            {missing > 0 ? (
              <Alert
                className="ed-message-warning"
                type="warning"
                showIcon
                title={`已返回 ${returnedIds.size} 条消息，缺少 ${missing} 条来源消息`}
                description="消息链路不完整，无法据此确认完整上下文。"
              />
            ) : null}
            {event.first_agent_reply_time !== null && anchorIndex < 0 ? (
              <Alert
                className="ed-message-warning"
                type="warning"
                showIcon
                title="原文中未找到匹配的首响锚点"
                description="首响时间与客服未在已返回消息中同时匹配。"
              />
            ) : null}
            <ol className="ed-thread">
              {ordered.map((m, index) => {
                const day = m.at.slice(0, 10);
                const newDay = index === 0 || ordered[index - 1]?.at.slice(0, 10) !== day;
                const internal = m.sender_role === "INTERNAL";
                const anchor = index === anchorIndex && merchant;
                const first = index === 0 && m.at === event.first_msg_time;
                return (
                  <Fragment key={m.msg_id}>
                    {newDay ? (
                      <li className="ed-day">
                        <time dateTime={day}>{day}</time>
                        <span>{weekdayOf(day)}</span>
                      </li>
                    ) : null}
                    <li
                      className="ed-message"
                      data-role={m.sender_role}
                      data-anchor={anchor}
                      ref={anchor ? anchorRef : undefined}
                      tabIndex={-1}
                    >
                      <span className="ed-sender-icon" aria-hidden="true">
                        {internal ? <CustomerServiceOutlined /> : <ShopOutlined />}
                      </span>
                      <article>
                        <header className="ed-message-heading">
                          <strong>
                            {internal
                              ? analytics.agentLabel(m.sender_id)
                              : `商家 ${shortId(m.sender_id, 8)}`}
                          </strong>
                          <span className="ed-role">{internal ? "平台客服" : "商家客服"}</span>
                          <time dateTime={m.at.replace(" ", "T")}>{m.at.slice(11)}</time>
                        </header>
                        {anchor || first ? (
                          <span className="ed-message-marker">
                            {anchor ? "首次有效回复" : internal ? "平台推送" : "诉求提出"}
                          </span>
                        ) : null}
                        <p className="ed-message-text">{m.text}</p>
                        {showIds ? (
                          <Typography.Text
                            className="ed-code ed-message-id"
                            copyable={{ text: m.msg_id }}
                          >
                            {m.msg_id}
                          </Typography.Text>
                        ) : null}
                      </article>
                    </li>
                  </Fragment>
                );
              })}
            </ol>
            <p className="ed-thread-end">
              <InfoCircleOutlined aria-hidden="true" /> 解决状态暂无数据来源
            </p>
          </>
        )}
      </div>
    </div>
  );
}
