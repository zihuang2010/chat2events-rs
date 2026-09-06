import { render } from "@testing-library/react";
import { StrictMode } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { EChart } from "./EChart";

const state = vi.hoisted(() => ({
  instances: [] as {
    disposed: boolean;
    off: ReturnType<typeof vi.fn>;
    resize: ReturnType<typeof vi.fn>;
  }[],
}));
vi.mock("echarts/core", () => ({
  use: vi.fn(),
  registerTheme: vi.fn(),
  setPlatformAPI: vi.fn(),
  init: () => {
    const instance = {
      disposed: false,
      dispose: () => {
        instance.disposed = true;
      },
      isDisposed: () => instance.disposed,
      on: vi.fn(),
      off: vi.fn(() => {
        expect(instance.disposed).toBe(false);
      }),
      resize: vi.fn(() => {
        expect(instance.disposed).toBe(false);
      }),
      setOption: vi.fn(),
    };
    state.instances.push(instance);
    return instance;
  },
}));
afterEach(() => {
  vi.unstubAllGlobals();
  state.instances.length = 0;
});

it("StrictMode 重挂载与卸载不操作已销毁的图表", () => {
  const observers: ResizeObserverCallback[] = [];
  vi.stubGlobal(
    "ResizeObserver",
    class {
      constructor(callback: ResizeObserverCallback) {
        observers.push(callback);
      }
      observe() {}
      disconnect() {}
    },
  );
  const view = render(
    <StrictMode>
      <EChart
        option={{}}
        height={100}
        ariaLabel="趋势"
        onEvent={{ type: "click", handler: () => {} }}
      />
    </StrictMode>,
  );
  expect(state.instances).toHaveLength(2);
  expect(state.instances[0]?.disposed).toBe(true);
  view.unmount();
  for (const callback of observers) callback([], {} as ResizeObserver);
  expect(state.instances.every((instance) => instance.disposed)).toBe(true);
  expect(state.instances.every((instance) => instance.resize.mock.calls.length === 0)).toBe(true);
});
