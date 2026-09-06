/** D 主题覆盖筛选条、上下文条和各视图，保持下钻后的观感一致。 */
import { ConfigProvider } from "antd";
import type { ReactNode } from "react";
import { WORKBENCH_THEME, antdThemeFor, cssVars } from "@/app/theme/workbench";
import "@/app/theme/fonts";
import "@/app/theme/workbench.css";
import "@/app/workbench.css";

const theme = antdThemeFor(WORKBENCH_THEME);
const variables = cssVars(WORKBENCH_THEME);

export function Workbench({ children }: { children: ReactNode }) {
  return (
    <ConfigProvider theme={theme} componentSize="small">
      <div className="pv pv-d" style={variables} data-skin="D">
        {children}
      </div>
    </ConfigProvider>
  );
}
