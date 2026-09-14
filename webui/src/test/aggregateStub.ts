/**
 * 视图测试的取数替身：让页面的聚合请求落到**这一组测试自己给的事件**上。
 *
 * 指标搬进 SQL 之后，页面不再从 `LoadedDataset` 里拿事件明细，而是打
 * `/api/summary`、`/api/rooms`、`/api/agents`、`/api/categories`、`/api/events`。
 * 视图测试要摆布数字，就得摆布这五个返回 —— 但**不能各自手写一份假数字**：
 * 那样测的就只是「我写的常量能不能渲染出来」，口径错了照样绿。
 *
 * 所以这里仍然走 `mock/aggregate`（口径的前端对照实现），只是把它的输入
 * 换成测试给的那批事件。渲染出来的每个数字仍然是**算出来的**。
 */
import type * as Source from "@/api/source";
import {
  mockAgentAggs,
  mockCategories,
  mockEventsPage,
  mockRoomAggs,
  mockSummary,
} from "@/api/mock/aggregate";
import type { EventRow, GroupDailyRow } from "@/domain/schemas";
import type { TaxonomyIndex } from "@/domain/metrics";

export interface AggregateStub {
  events: EventRow[];
  groupDaily: GroupDailyRow[];
  tax: TaxonomyIndex;
}

/**
 * 交给 `vi.mock("@/api/source", ...)` 的工厂。只替换五个聚合读取，
 * 其余（`loadDataset` / `loadMessages` / 类型）原样透传。
 */
export function sourceStub(actual: typeof Source, stub: AggregateStub): typeof Source {
  const input = () => [stub.events, stub.groupDaily, stub.tax] as const;
  return {
    ...actual,
    loadSummary: (_source, f) => Promise.resolve(mockSummary(...input(), f)),
    loadRoomAggs: (_source, f, groups) => Promise.resolve(mockRoomAggs(...input(), f, groups)),
    loadAgentAggs: (_source, f) => Promise.resolve(mockAgentAggs(...input(), f)),
    loadCategories: (_source, f, groups) => Promise.resolve(mockCategories(...input(), f, groups)),
    loadEventsPage: (_source, f, page, size, sorting) =>
      Promise.resolve(mockEventsPage(...input(), f, page, size, sorting)),
  };
}
