import { renderHook } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import { useColorScheme } from "./useColorScheme";

it("旧深色偏好和系统深色设置不影响固定浅色主题", () => {
  localStorage.setItem("c2e.color-preference", "dark");
  const matchMedia = vi.fn(() => ({ matches: true }));
  vi.stubGlobal("matchMedia", matchMedia);
  try {
    const { result, unmount } = renderHook(useColorScheme);
    expect(result.current.mode).toBe("light");
    expect(document.documentElement.dataset["theme"]).toBe("light");
    expect(document.documentElement.style.getPropertyValue("color-scheme")).toBe("light");
    expect(matchMedia).not.toHaveBeenCalled();
    unmount();
  } finally {
    localStorage.removeItem("c2e.color-preference");
    vi.unstubAllGlobals();
  }
});
