import "../filters/filters.css";
import { Button, DatePicker, Form, Input, Segmented, Select } from "antd";
import {
  CloseCircleOutlined,
  CloseOutlined,
  DownOutlined,
  ReloadOutlined,
  SearchOutlined,
} from "@ant-design/icons";
import dayjs, { type Dayjs } from "dayjs";
import { useState } from "react";
import type { Meta } from "@/domain/schemas";
import { addDays, windowBounds } from "@/lib/format";
import { DEFAULT_SLA_SEC, EVENT_STATUS, STATUS_FILTERS, UNTYPED } from "@/domain/definitions";
import type { FilterPatch, FiltersApi } from "@/features/filters/useFilters";

export function RoomFilters({
  meta,
  api,
  label = "群聊洞察筛选条件",
}: {
  meta: Meta;
  api: FiltersApi;
  label?: string;
}) {
  const { filters, patch, reset } = api;
  const first = meta.days[0] as string;
  const last = meta.days.at(-1) as string;
  const { from, to } = windowBounds(meta.days, filters.from, filters.to);
  const defaults = windowBounds(meta.days);
  const threeDaysFrom = addDays(last, -2) < first ? first : addDays(last, -2);
  const [expanded, setExpanded] = useState(false);
  const [search, setSearch] = useState({ applied: filters.query, draft: filters.query });
  // 输入草稿保留原来的回车提交行为；URL 导航与重置同步已提交关键词。
  if (search.applied !== filters.query) {
    setSearch({ applied: filters.query, draft: filters.query });
  }
  const untypedLabel = meta.taxonomy_version === "v0" ? "未建词表" : "未归类 / 归不上去";
  // 折叠区里生效的条件在收起时也要看得见：看不见的筛选会把读数悄悄改掉，
  // 而用户只看到一个「看起来合理」的数字。每个条件一颗可点掉的筹码。
  const level2 = meta.taxonomy.find((type) => type.type_id === filters.level2);
  const moreChips: { key: string; label: string; clear: FilterPatch }[] = [];
  if (filters.level1) {
    moreChips.push({ key: "一级", label: filters.level1, clear: { level1: null } });
  }
  if (filters.level2) {
    moreChips.push({
      key: "二级",
      label: level2 ? `${level2.parent_name} / ${level2.name}` : untypedLabel,
      clear: { level2: null },
    });
  }
  if (filters.status) {
    moreChips.push({ key: "状态", label: EVENT_STATUS[filters.status], clear: { status: null } });
  }
  if (filters.overdueOnly !== null) {
    moreChips.push({
      key: "超时",
      label: filters.overdueOnly ? "仅超时" : "仅未超时",
      clear: { overdueOnly: null },
    });
  }
  const moreCount = moreChips.length;
  const active =
    from !== defaults.from ||
    to !== defaults.to ||
    Boolean(filters.room || filters.agent || filters.query.trim() || moreCount) ||
    filters.slaSec !== DEFAULT_SLA_SEC;
  const preset =
    from === to && to === last
      ? "1d"
      : to === last && from === threeDaysFrom
        ? "3d"
        : to === last && from === defaults.from
          ? "7d"
          : "custom";
  const disabledDate = (date: Dayjs) => {
    const value = date.format("YYYY-MM-DD");
    return value < first || value > last;
  };

  return (
    <Form
      component="div"
      layout="vertical"
      size="middle"
      className="ra-filter-panel"
      aria-label={label}
    >
      <div className="ra-filter-primary">
        <div className="ra-filter-search-actions">
          <Form.Item label="关键词" htmlFor="room-filter-query" className="ra-filter-search">
            <Input.Search
              id="room-filter-query"
              aria-label="关键词"
              allowClear={{ clearIcon: <CloseCircleOutlined aria-label="清除" /> }}
              placeholder="搜索关键词"
              title="事件摘要、分类、群聊或客服"
              enterButton={<Button aria-label="搜索" title="搜索" icon={<SearchOutlined />} />}
              value={search.draft}
              onChange={(event) => setSearch({ applied: filters.query, draft: event.target.value })}
              onSearch={(value) => patch({ query: value })}
            />
          </Form.Item>
          <div className="ra-filter-actions">
            <Button
              className="ra-more-toggle"
              data-active={moreCount > 0}
              aria-label={`更多筛选${moreCount ? `（${moreCount}）` : ""}`}
              aria-expanded={expanded}
              aria-controls="room-filter-more"
              onClick={() => setExpanded(!expanded)}
            >
              更多筛选
              <span className="ra-more-count" data-empty={moreCount === 0} aria-hidden="true">
                {moreCount}
              </span>
              <DownOutlined aria-hidden="true" rotate={expanded ? 180 : 0} />
            </Button>
            <Button
              type="text"
              className="ra-filter-reset"
              icon={<ReloadOutlined aria-hidden="true" />}
              data-active={active}
              onClick={() => {
                setSearch({ applied: "", draft: "" });
                reset();
              }}
            >
              重置
            </Button>
          </div>
        </div>
        <div className="ra-filter-dates">
          <Form.Item label="日期范围" htmlFor="room-filter-from">
            <DatePicker.RangePicker
              classNames={{ popup: { root: "ra-range-popup" } }}
              id={{ start: "room-filter-from", end: "room-filter-to" }}
              aria-label="日期范围"
              value={[dayjs(from), dayjs(to)]}
              allowClear={false}
              disabledDate={disabledDate}
              onChange={(range) => {
                const [a, b] = range ?? [];
                if (a && b) patch({ from: a.format("YYYY-MM-DD"), to: b.format("YYYY-MM-DD") });
              }}
            />
          </Form.Item>
          <Form.Item label="快捷时间" htmlFor="room-filter-period">
            <Segmented
              id="room-filter-period"
              aria-label="快捷时间"
              value={preset}
              options={[
                { label: "最后 1 天", value: "1d" },
                { label: "近 3 天", value: "3d" },
                { label: "近 7 天", value: "7d" },
              ]}
              onChange={(value) => {
                if (value === "7d") patch(defaults);
                if (value === "3d") patch({ from: threeDaysFrom, to: last });
                if (value === "1d") patch({ from: last, to: last });
              }}
            />
          </Form.Item>
        </div>
        <div className="ra-filter-people">
          <Form.Item label="群聊" htmlFor="room-filter-room">
            <Select
              id="room-filter-room"
              aria-label="群聊"
              allowClear
              showSearch
              optionFilterProp="label"
              placeholder={`全部群（${meta.rooms.length}）`}
              value={filters.room}
              onChange={(value: string | undefined) => patch({ room: value ?? null })}
              options={meta.rooms.map((room) => ({
                value: room.roomid,
                label: room.alias ?? room.roomid,
              }))}
            />
          </Form.Item>
          <Form.Item label="客服" htmlFor="room-filter-agent">
            <Select
              id="room-filter-agent"
              aria-label="客服"
              allowClear
              showSearch
              optionFilterProp="label"
              placeholder={`全部客服（${meta.agents.length}）`}
              value={filters.agent}
              onChange={(value: string | undefined) =>
                patch({ agent: value ?? null, focusAgent: value ?? null })
              }
              options={meta.agents.map((agent) => ({
                value: agent.agent,
                label: agent.alias ?? agent.agent,
              }))}
            />
          </Form.Item>
        </div>
      </div>
      {!expanded && moreCount > 0 ? (
        <div className="ra-filter-chips">
          <span className="ra-filter-chips-label">更多筛选生效中</span>
          {moreChips.map((chip) => (
            <Button
              key={chip.key}
              size="small"
              className="ra-filter-chip"
              aria-label={`清除${chip.key}筛选：${chip.label}`}
              onClick={() => patch(chip.clear)}
            >
              <span className="ra-chip-key">{chip.key}</span>
              {chip.label}
              <CloseOutlined aria-hidden="true" />
            </Button>
          ))}
        </div>
      ) : null}
      <div id="room-filter-more" className="ra-filter-more" hidden={!expanded}>
        <Form.Item label="一级分类" htmlFor="room-filter-level1">
          <Select
            id="room-filter-level1"
            aria-label="一级分类"
            allowClear
            placeholder="全部一级"
            value={filters.level1}
            onChange={(value: string | undefined) => patch({ level1: value ?? null, level2: null })}
            options={[...new Set(meta.taxonomy.map((type) => type.parent_name))].map((value) => ({
              value,
              label: value,
            }))}
          />
        </Form.Item>
        <Form.Item label="二级分类" htmlFor="room-filter-level2">
          <Select
            id="room-filter-level2"
            aria-label="二级分类"
            allowClear
            showSearch
            optionFilterProp="label"
            placeholder="全部二级"
            value={filters.level2}
            onChange={(value: string | undefined) =>
              patch({ level2: value ?? null, ...(value ? { level1: null } : {}) })
            }
            options={[
              ...meta.taxonomy.map((type) => ({
                value: type.type_id,
                label: `${type.parent_name} / ${type.name}`,
              })),
              { value: UNTYPED, label: untypedLabel },
            ]}
          />
        </Form.Item>
        <Form.Item label="状态" htmlFor="room-filter-status">
          <Select
            id="room-filter-status"
            aria-label="状态"
            allowClear
            placeholder="全部状态"
            value={filters.status}
            onChange={(value: typeof filters.status | undefined) =>
              patch({ status: value ?? null })
            }
            options={STATUS_FILTERS.map((status) => ({
              value: status,
              label: EVENT_STATUS[status],
            }))}
          />
        </Form.Item>
        <Form.Item label="超时条件" htmlFor="room-filter-overdue">
          <Select
            id="room-filter-overdue"
            aria-label="超时条件"
            allowClear
            placeholder="超时不限"
            value={filters.overdueOnly === null ? undefined : filters.overdueOnly ? "1" : "0"}
            onChange={(value: string | undefined) =>
              patch({ overdueOnly: value === undefined ? null : value === "1" })
            }
            options={[
              { value: "1", label: "仅超时" },
              { value: "0", label: "仅未超时" },
            ]}
          />
        </Form.Item>
      </div>
    </Form>
  );
}
