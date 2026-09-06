/**
 * 页尾：**能力边界白纸黑字写清楚**。
 * 给上级看的看板最危险的不是数字错，是没人知道哪些数字根本不存在。
 */

import { Alert, Collapse, Table, Typography } from "antd";
import { DATA_GAPS, PITFALLS } from "@/domain/definitions";
import type { SourceKind } from "@/api/source";
import { Link, useLocation } from "react-router-dom";

export function CapabilityFooter({
  source,
  fallbackReason,
}: {
  source: SourceKind;
  fallbackReason: string | null;
}) {
  const location = useLocation();
  const params = new URLSearchParams(location.search);
  params.set("source", "api");
  return (
    <footer style={{ maxWidth: 1760, margin: "0 auto", padding: "0 16px 40px", width: "100%" }}>
      {source === "mock" ? (
        <Alert
          type="warning"
          showIcon
          style={{ marginBottom: 12 }}
          title="当前是模拟数据源，页面上的指标不是真实统计"
          description={
            <>
              {fallbackReason}
              <br />
              真接口就绪后无需改前端：把 <Typography.Text code>/api/*</Typography.Text> 反代到只读
              JSON 服务即可，字段名与 <Typography.Text code>schema.sql</Typography.Text> 逐字对齐。
              也可以用{" "}
              <Link to={{ pathname: location.pathname, search: `?${params}`, hash: location.hash }}>
                ?source=api
              </Link>{" "}
              强制只走真接口（失败即报错，不回落）。
            </>
          }
        />
      ) : null}

      <Collapse
        size="small"
        defaultActiveKey={["gaps"]}
        items={[
          {
            key: "gaps",
            label: <b>待补齐的数据能力（{DATA_GAPS.length} 项，界面上一律标注，不编造）</b>,
            children: (
              <Table
                size="small"
                rowKey="title"
                pagination={false}
                scroll={{ x: 720 }}
                dataSource={[...DATA_GAPS]}
                columns={[
                  { title: "能力", dataIndex: "title", key: "title", width: 200 },
                  { title: "为什么现在没有", dataIndex: "detail", key: "detail" },
                  { title: "补齐需要什么", dataIndex: "needs", key: "needs", width: 320 },
                ]}
              />
            ),
          },
          {
            key: "pitfalls",
            label: <b>三个会静默给出偏小数字的口径洞，以及本看板的处理方式</b>,
            children: (
              <Table
                size="small"
                rowKey="title"
                pagination={false}
                scroll={{ x: 720 }}
                dataSource={[...PITFALLS]}
                columns={[
                  { title: "洞", dataIndex: "title", key: "title", width: 160 },
                  { title: "风险", dataIndex: "risk", key: "risk" },
                  { title: "处理", dataIndex: "handling", key: "handling", width: 380 },
                ]}
              />
            ),
          },
          {
            key: "notes",
            label: <b>口径备注</b>,
            children: (
              <Typography.Paragraph style={{ marginBottom: 0, lineHeight: 1.9 }}>
                分位数一律在事件明细上现算，不对每日 p50 取平均（分位数不可加）。 分类只按主类{" "}
                <Typography.Text code>event_type</Typography.Text> 统计， 副类只落{" "}
                <Typography.Text code>event_types</Typography.Text> 列供下钻。
                全站走自然时间口径（UTC+8），跨夜与周末照常计时。
              </Typography.Paragraph>
            ),
          },
        ]}
      />
    </footer>
  );
}
