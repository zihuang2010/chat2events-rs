/**
 * 全应用一个查询缓存实例：跨页面切换时同一份数据不重复回源。
 *
 * ⚠️ 单独成文件是为了**能在测试里 `clear()`**：它是模块级单例，多个用例共用同一份
 * 缓存，而聚合查询的 key 只含筛选条件、不含「这一组用例喂了什么数据」——
 * 不在用例之间清，后一个用例会读到前一个的结果，且看起来完全正常。
 */
import { QueryClient } from "@tanstack/react-query";

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // 每日跑批：数据一天只换一次，没必要反复回源
      staleTime: 5 * 60_000,
      refetchOnWindowFocus: false,
      retry: 1,
    },
  },
});
