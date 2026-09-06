/** 工作台固定浅色，旧偏好与系统深色设置不再影响页面。 */
import { useLayoutEffect } from "react";
import { applyCssVariables } from "./tokens";

export interface ColorScheme {
  mode: "light";
}

const SCHEME: ColorScheme = { mode: "light" };

export function useColorScheme(): ColorScheme {
  useLayoutEffect(() => {
    applyCssVariables();
    document.documentElement.dataset["theme"] = SCHEME.mode;
  }, []);

  return SCHEME;
}
