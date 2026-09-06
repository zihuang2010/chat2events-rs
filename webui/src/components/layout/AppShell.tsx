/**
 * 应用外壳：品牌、视图导航、数据源标记。
 *
 * **数据源标记常驻顶栏**：只要还在用模拟数据，任何时候截图给上级看，
 * 这个标记都在画面里，不会有人把演示数据当成真实统计。
 */

import { Tag, Tooltip } from "antd";
import { NavLink, useLocation } from "react-router-dom";
import type { ReactNode } from "react";
import { NAV } from "@/domain/definitions";
import type { SourceKind } from "@/api/source";

export function AppShell({
  source,
  fallbackReason,
  children,
}: {
  source: SourceKind | undefined;
  fallbackReason: string | null | undefined;
  children: ReactNode;
}) {
  const { search } = useLocation();

  return (
    <>
      <header className="c2e-topbar">
        <div className="c2e-brand">
          <b>群聊事件与客服效率</b>
          {source === "mock" ? (
            <Tooltip title={`${fallbackReason ?? ""} 页面上的指标不是真实统计。`}>
              <Tag color="warning" style={{ marginInlineEnd: 0 }}>
                模拟数据
              </Tag>
            </Tooltip>
          ) : source === "api" ? (
            <Tooltip title="数据来自只读 JSON 接口 /api/*">
              <Tag color="success" style={{ marginInlineEnd: 0 }}>
                真实接口
              </Tag>
            </Tooltip>
          ) : null}
        </div>

        <nav className="c2e-nav" aria-label="分析视图">
          {NAV.map((item) => (
            <NavLink key={item.key} to={{ pathname: item.path, search }} className="c2e-nav-link">
              {item.label}
            </NavLink>
          ))}
        </nav>
      </header>
      {children}
    </>
  );
}
