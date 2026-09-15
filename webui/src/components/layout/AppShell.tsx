/** 应用外壳：品牌与视图导航。 */

import { NavLink, useLocation } from "react-router-dom";
import type { ReactNode } from "react";
import { NAV } from "@/domain/definitions";
import { freshnessNote, weekdayOf } from "@/lib/format";

export function AppShell({ children, asOf }: { children: ReactNode; asOf?: string | undefined }) {
  const { search } = useLocation();
  // 数据还没加载出来（骨架 / 报错态）时没有截至日可报，整块不渲染。
  const note = asOf ? freshnessNote(asOf) : null;

  return (
    <>
      <header className="c2e-topbar">
        <div className="c2e-brand">
          <b>群聊事件与客服效率</b>
        </div>

        <nav className="c2e-nav" aria-label="分析视图">
          {NAV.map((item) => (
            <NavLink key={item.key} to={{ pathname: item.path, search }} className="c2e-nav-link">
              {item.label}
            </NavLink>
          ))}
        </nav>

        {asOf ? (
          <span className="c2e-asof" data-stale={note?.stale ?? false}>
            数据截至 {asOf}（{weekdayOf(asOf)}）{note ? <em>· {note.text}</em> : null}
          </span>
        ) : null}
      </header>
      {children}
    </>
  );
}
