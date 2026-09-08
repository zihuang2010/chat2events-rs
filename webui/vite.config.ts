import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath, URL } from "node:url";

/**
 * 构建产物是纯静态文件，由 nginx 之类的静态服务器托管，`/api/*` 反向代理到
 * 只读 JSON 服务（见 deploy/nginx.conf）。前端不关心后端部署在哪。
 *
 * VITE_BASE      子路径部署时设置，默认根路径
 * VITE_API_PROXY 开发期把 /api 转发到哪，默认 http://127.0.0.1:8787
 */
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "");
  return {
    base: env.VITE_BASE || "/",
    plugins: [react()],
    resolve: {
      alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
    },
    server: {
      port: 5273,
      proxy: {
        "/api": {
          target: env.VITE_API_PROXY || "http://127.0.0.1:8787",
          changeOrigin: true,
        },
      },
    },
    build: {
      outDir: "dist",
      sourcemap: true,
      chunkSizeWarningLimit: 900,
      rollupOptions: {
        output: {
          // 图表和 React 分开缓存；AntD 按实际路由依赖自动分块。
          // 用函数形式而不是对象形式：对象形式在当前打包器上已不受支持。
          manualChunks(id: string): string | undefined {
            if (!id.includes("node_modules")) return undefined;
            if (id.includes("echarts") || id.includes("zrender")) return "echarts";
            if (id.includes("react-router") || id.includes("/react-dom/") || id.includes("/react/")) {
              return "react";
            }
            return undefined;
          },
        },
      },
    },
  };
});
