import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";

// 自托管字体：不从公网 CDN 拉，内网部署也能拿到正确字形。
// 中文回落系统字体，不打包 CJK 字重（那是好几 MB）。
import "@fontsource/ibm-plex-sans/400.css";
import "@fontsource/ibm-plex-sans/500.css";
import "@fontsource/ibm-plex-sans/600.css";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";
import "@fontsource/ibm-plex-mono/600.css";

import App from "./App";
import { Providers } from "./app/providers";
import { ErrorBoundary } from "./app/ErrorBoundary";
import "./app/global.css";

const container = document.getElementById("root");
if (!container) throw new Error("找不到挂载点 #root");

createRoot(container).render(
  <StrictMode>
    <BrowserRouter basename={import.meta.env.BASE_URL}>
      <Providers>
        <ErrorBoundary>
          <App />
        </ErrorBoundary>
      </Providers>
    </BrowserRouter>
  </StrictMode>,
);
