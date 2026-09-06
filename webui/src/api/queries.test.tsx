import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, renderHook, waitFor } from "@testing-library/react";
import { useState, type ReactNode } from "react";
import { MemoryRouter, useNavigate } from "react-router-dom";
import { act } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { useDataset, useEventMessages } from "./queries";
import { loadDataset, loadMessages, type SourceKind } from "./source";

vi.mock("./source", () => ({
  loadDataset: vi.fn((source: string) => Promise.resolve({ source })),
  loadMessages: vi.fn((source: string) => Promise.resolve([{ msg_id: source }])),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function Wrapper({ children }: { children: ReactNode }) {
  const [client] = useState(
    () => new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: Infinity } } }),
  );
  return (
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={["/detail?source=mock"]}>{children}</MemoryRouter>
    </QueryClientProvider>
  );
}

it("相同事件 ID 切换数据源后重新取消息，不复用另一数据源原文", async () => {
  const { result, rerender } = renderHook(
    ({ source }: { source: SourceKind }) => useEventMessages(source, 1),
    {
      initialProps: { source: "mock" },
      wrapper: Wrapper,
    },
  );
  await waitFor(() => expect(result.current.data?.[0]?.msg_id).toBe("mock"));
  rerender({ source: "api" });
  await waitFor(() => expect(result.current.data?.[0]?.msg_id).toBe("api"));
  expect(loadMessages).toHaveBeenCalledWith("api", 1);
});

it("URL 数据源改变后重新装载数据集", async () => {
  const { result } = renderHook(() => ({ query: useDataset(), navigate: useNavigate() }), {
    wrapper: Wrapper,
  });
  await waitFor(() => expect(result.current.query.data?.source).toBe("mock"));
  await act(async () => {
    await result.current.navigate("/detail?source=api");
  });
  await waitFor(() => expect(result.current.query.data?.source).toBe("api"));
  expect(loadDataset).toHaveBeenCalledWith("api");
});
