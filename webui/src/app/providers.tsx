import { QueryClientProvider } from "@tanstack/react-query";
import { App as AntApp, ConfigProvider } from "antd";
import zhCN from "antd/locale/zh_CN";
import dayjs from "dayjs";
import "dayjs/locale/zh-cn";
import type { ReactNode } from "react";
import { queryClient } from "./queryClient";
import { buildAntdTheme } from "./theme/antdTheme";
import { registerEchartsThemes } from "./theme/echartsTheme";
import { useColorScheme } from "./theme/useColorScheme";

dayjs.locale("zh-cn");
registerEchartsThemes();
const theme = buildAntdTheme();

export function Providers({ children }: { children: ReactNode }) {
  useColorScheme();

  return (
    <QueryClientProvider client={queryClient}>
      <ConfigProvider locale={zhCN} theme={theme} componentSize="small">
        <AntApp>{children}</AntApp>
      </ConfigProvider>
    </QueryClientProvider>
  );
}
