import { Typography } from "antd";
import type { ReactNode } from "react";
import type { DecoratedEvent } from "@/domain/schemas";
import { isMerchant, isOverdue } from "@/domain/metrics";
import { DataGap } from "@/components/primitives";
import type { Analytics } from "@/features/filters/useAnalytics";
import { formatDuration, weekdayOf } from "@/lib/format";

interface EvidenceProps {
  event: DecoratedEvent;
  analytics: Analytics;
}

export function EventProperties({ event, analytics }: EvidenceProps) {
  const { roomLabel, agentLabel, roomAliasIsAuthoritative, taxIndex } = analytics;
  return (
    <div className="ed-evidence">
      <section>
        <h3>事件摘要</h3>
        <p className="ed-full-summary">{event.summary}</p>
      </section>
      <section>
        <h3>来源与参与方</h3>
        <dl className="ed-fields">
          <Field label="所属群">
            {roomLabel(event.roomid)}{" "}
            {!roomAliasIsAuthoritative(event.roomid) ? (
              <DataGap label="别名待补" detail="尚未获取到该群的权威名称。" />
            ) : null}
            <Typography.Text className="ed-code" copyable={{ text: event.roomid }}>
              {event.roomid}
            </Typography.Text>
          </Field>
          <Field label="提出方">
            {isMerchant(event) ? "商家客服" : "平台（工单推送）"}
            <Typography.Text className="ed-code" copyable={{ text: event.asker }}>
              {event.asker}
            </Typography.Text>
          </Field>
          <Field label="首响客服">
            {event.first_responder ? agentLabel(event.first_responder) : "无响应，不归属任何客服"}
          </Field>
          <Field label="活跃客服">
            {event.agents.length ? event.agents.map(agentLabel).join("、") : "无"}
          </Field>
        </dl>
      </section>
      <section>
        <h3>分类与时间</h3>
        <dl className="ed-fields">
          <Field label="主分类">
            {event.level1} / {event.level2}
          </Field>
          <Field label="副分类">
            {event.event_types && event.event_types.length > 1
              ? event.event_types
                  .slice(1)
                  .map((type) => taxIndex.get(type)?.name ?? type)
                  .join("、")
              : event.event_types === null
                ? "打标未完成"
                : "无"}
            <span className="ed-field-note">副类不计入指标</span>
          </Field>
          <Field label="词表版本">
            <span className="ed-mono">{event.taxonomy_version ?? "打标未完成"}</span>
          </Field>
          <Field label="归属日">
            {event.occurred_on} {weekdayOf(event.occurred_on)}
            <span className="ed-field-note">按首条消息日期计入，跨天事件只计一次</span>
          </Field>
          <Field label="开始时间">
            <span className="ed-mono">{event.first_msg_time}</span>
          </Field>
          <Field label="首响时间">
            <span className="ed-mono">{event.first_agent_reply_time ?? "NULL（无响应）"}</span>
          </Field>
          <Field label="来源消息">{event.source_msg_ids.length} 条</Field>
          <Field label="已解决">
            <DataGap detail="没有 resolved 字段与解决判定口径。" />
          </Field>
        </dl>
      </section>
    </div>
  );
}

export function EventBasis({ event, analytics }: EvidenceProps) {
  const merchant = isMerchant(event);
  return (
    <div className="ed-evidence">
      <section>
        <h3>首响计算</h3>
        <div className="ed-calculation">
          <div>
            <span>首次有效回复</span>
            <strong>{event.first_agent_reply_time ?? "NULL"}</strong>
          </div>
          <span className="ed-operator" aria-hidden="true">
            −
          </span>
          <div>
            <span>首条消息</span>
            <strong>{event.first_msg_time}</strong>
          </div>
          <span className="ed-operator" aria-hidden="true">
            =
          </span>
          <div>
            <span>首响耗时</span>
            <strong>{formatDuration(event.firstReplySec) ?? "不计算"}</strong>
          </div>
        </div>
        <p className="ed-field-note">
          first_agent_reply_time − first_msg_time，按自然时间计算，时区 UTC+8。
        </p>
        {event.firstReplySec === null ? (
          <p className="ed-basis-note">无响应事件不以 0 秒计入均值或分位数。</p>
        ) : null}
        {!merchant ? (
          <p className="ed-basis-note">平台推送首响记录为 0 秒，但不计入首响分位数或无响应率。</p>
        ) : null}
      </section>
      <section>
        <h3>统计归属</h3>
        <dl className="ed-fields">
          <Field label="首响口径">
            <strong>{merchant ? "纳入商家发起分母" : "不纳入首响指标"}</strong>
            <span className="ed-field-note">
              asker_role = {event.asker_role}；
              {merchant ? "计入 merchant_event_count" : "平台发起事件排除"}
            </span>
          </Field>
          <Field label="超时判定">
            <strong className="ed-verdict" data-overdue={isOverdue(event, analytics.slaSec)}>
              {merchant ? (isOverdue(event, analytics.slaSec) ? "超时" : "未超时") : "不适用"}
            </strong>
            <span className="ed-field-note">
              当前阈值 {formatDuration(analytics.slaSec)}，自然时间口径
            </span>
          </Field>
          <Field label="处理量">
            {event.first_responder ? (
              <>
                计入 <strong>{analytics.agentLabel(event.first_responder)}</strong>
              </>
            ) : (
              <strong>不计入任何客服</strong>
            )}
            <span className="ed-field-note">
              按 first_responder 归属至 agent_metric_daily；缺少首响客服时不分摊处理量。
            </span>
          </Field>
          <Field label="活跃量">
            {event.agents.length} 位客服各自计入
            <span className="ed-field-note">agents[] 记录参与方，不能用活跃量代替首响处理量。</span>
          </Field>
          <Field label="溯源依据">
            {event.source_msg_ids.length} 条 source_msg_ids
            <span className="ed-field-note">
              消息原文需同时匹配首响时间、平台客服身份及 first_responder，才能确认首响锚点。
            </span>
          </Field>
          <Field label="解决节点">
            <DataGap />
            <span className="ed-field-note">
              没有 resolved 字段与解决判定口径；still_open
              仅是抽取模型的段内控制位，未写入事件记录。
            </span>
          </Field>
        </dl>
      </section>
    </div>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="ed-field">
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}
