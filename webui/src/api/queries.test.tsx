import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, renderHook, waitFor } from "@testing-library/react";
import { useState, type ReactNode } from "react";
import { MemoryRouter, useNavigate } from "react-router-dom";
import { act } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { useDataset, useEventMessages } from "./queries";
import { loadDataset, loadMessages, type SourceKind } from "./source";

vi.mock("./source", () => ({
  loadDataset: vi.fn(() => Promise.resolve({ source: "api" })),
  loadMessages: vi.fn((eventId: number) => Promise.resolve([{ msg_id: String(eventId) }])),
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

it("loads_messages_only_after_dataset_is_ready", async () => {
  const initialProps: { source: SourceKind | undefined } = { source: undefined };
  const { result, rerender } = renderHook(
    ({ source }: { source: SourceKind | undefined }) => useEventMessages(source, 1),
    { initialProps, wrapper: Wrapper },
  );
  expect(loadMessages).not.toHaveBeenCalled();
  rerender({ source: "api" });
  await waitFor(() => expect(result.current.data?.[0]?.msg_id).toBe("1"));
  expect(loadMessages).toHaveBeenCalledWith(1);
});

it("ignores_legacy_source_selection", async () => {
  const { result } = renderHook(() => ({ query: useDataset(), navigate: useNavigate() }), {
    wrapper: Wrapper,
  });
  await waitFor(() => expect(result.current.query.data?.source).toBe("api"));
  expect(loadDataset).toHaveBeenCalledWith({ from: null, to: null });
  vi.mocked(loadDataset).mockClear();
  await act(async () => {
    await result.current.navigate("/detail?source=api");
  });
  expect(loadDataset).not.toHaveBeenCalled();
});

it("reloads_for_date_changes_but_not_drawer_changes", async () => {
  const { result } = renderHook(() => ({ query: useDataset(), navigate: useNavigate() }), {
    wrapper: Wrapper,
  });
  await waitFor(() => expect(result.current.query.data?.source).toBe("api"));
  await act(async () => {
    await result.current.navigate("/detail?from=2026-08-25&to=2026-08-26");
  });
  await waitFor(() =>
    expect(loadDataset).toHaveBeenCalledWith({ from: "2026-08-25", to: "2026-08-26" }),
  );
  vi.mocked(loadDataset).mockClear();
  await act(async () => {
    await result.current.navigate("/detail?from=2026-08-25&to=2026-08-26&drawer=1");
  });
  expect(loadDataset).not.toHaveBeenCalled();
});
