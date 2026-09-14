# WebUI 工程化与前端审核

审核日期：2026-09-06。范围为 `webui/src` 全部生产模块、样式、测试、依赖声明、构建配置、部署示例及前端 CI 接入。
以已确认页面和 `design.md` 为设计基线，未重设计页面或新增业务功能。

## 已修复的问题

| 级别 | 问题与影响 | 修复位置 |
| --- | --- | --- |
| P1 | 消息缓存仅按事件 ID 索引，模拟源与真实源可能串用原文；数据集缓存也没有响应 URL 数据源变化 | `src/api/queries.ts`：数据集按选择的数据源缓存，消息按实际数据源与事件 ID 缓存 |
| P1 | 概览与重置清空 `source`，刷新后可能改变取数方式；数据源切换丢失当前筛选 | `OverviewPage.tsx`、`useFilters.ts`、`CapabilityFooter.tsx`：保留数据源选择和筛选 |
| P1 | 所有读取异常都回落模拟源，真实服务契约错误、权限错误或部分读取失败无法显式呈现 | `src/api/source.ts`：只在探活不可达、超时、404、5xx 时允许默认模式回落，后续失败直接报错 |
| P1 | 请求收到响应头就清除超时，正文停滞会无限等待；无效 JSON 未归类为契约错误 | `src/api/client.ts`：超时覆盖正文读取，HTTP、网络、超时和契约错误分别报告 |
| P1 | 群筛选仅作用于事件，群表继续列出其他群并显示零事件 | `RoomsPage.tsx`：群名册与事件使用相同的群条件 |
| P1 | DATETIME 按浏览器本地时区计算，遇到夏令时切换可能误差一小时 | `src/lib/format.ts`：显式 UTC+8 解析和格式化，日历运算使用 UTC |
| P1 | 契约接受无效日期、倒序元数据、错误首响归属、时间顺序及失败日零值，可能静默产生错误指标 | `src/domain/schemas.ts`：补充有效日期及已有领域约束校验，不改变字段形状 |
| P1 | 非概览页在全范围抽取失败时显示零事件 | `InsightsLayout.tsx` 及三个消费页面：事件指标显示暂缺，禁止无依据的响应构成 |
| P2 | 群分类使用手写 `<a>`，子路径部署与修饰键打开链接不正确 | `RoomsPage.tsx`：使用 React Router `Link` |
| P2 | 群指标抽屉就地挂载，未获得标准 Portal 的背景滚动锁定 | `RoomInsightsDrawer.tsx`：使用标准 Portal 并显式继承工作台令牌 |
| P2 | 图表卸载先销毁实例、后解绑事件，触发已销毁实例操作；布局回调也可能迟到 | `EChart.tsx`：清理与尺寸回调检查实例生命周期，补 StrictMode 回归 |
| P2 | 小数分页参数可能解析为零；非法日期导致控件与指标口径不一致 | `useFilters.ts`：接受正安全整数，规范化日期和倒序区间 |
| P2 | 概览事件被错误标为筛选外；等待时长与末日 24:00 文案相差一秒 | `EventDrawer.tsx`、`overviewMetrics.ts`：区分无翻页位置与筛选外，修正末日边界 |
| P2 | 声明的 Node.js 20 不兼容现有测试工具，仓库 CI 不检查前端 | `package.json`、`../.github/workflows/ci.yml`：固定 pnpm 11.21.0，前端检查使用 Node.js 24 |

## 工程化与清理

- 将 `prototype/VariantD*` 整理为 `OverviewDashboard.tsx`、`OverviewCharts.tsx`、`overviewMetrics.ts` 和 `OverviewPage.test.tsx`。
- 将工作台主题、字体、语义 CSS 归入 `app/theme`；共享布局放入 `app/workbench.css`，外壳组件为 `components/layout/Workbench.tsx`。
- 保留已确认的外壳与页面视觉差异，删除不可达的深色主题和主题上下文，不引入主题切换机制。
- 删除七个旧组件：`CoverageAlert`、`Panel`、`KpiRow`、`HeatmapTable`、`RankBarChart`、`SmallMultiples`、`FilterBar`。旧 KPI 类型由实际消费模块持有。
- 删除旧排行、热力图、审计详情、消息模板和筛选条的无引用样式；删除旧原语及无界面消费的环比、热力矩阵、密度和格式化辅助计算。
- 删除原型说明和过期设计阶段日志。只撤除退役实现的测试，首响归属、失败日 NULL、消息分母等仍由当前汇总和模拟数据测试覆盖。
- 五个路由与模拟数据生成器按需加载；增加渲染和资源加载错误边界；移除未使用的 ECharts 插件，注册散点标签避让功能。
- 源码与样式相比操作前快照净减少约 1,000 行，包含新增回归测试。源码导入遍历未发现不可达的生产模块或失效的相对引用。
- 生产依赖没有新增或升级，`pnpm-lock.yaml` 与操作前完全一致；现有字体均仍有消费点，保持自托管。

## 验证结果

| 验证 | 结果 |
| --- | --- |
| 初始 `pnpm check` | 112 个测试通过；存在 jsdom Canvas / 伪元素能力警告 |
| 最终 `pnpm check` | lint、格式、类型检查及 16 个文件的 136 个测试通过，无上述警告 |
| 时区回归 | `TZ=America/New_York pnpm exec vitest run src/lib/format.test.ts src/domain/metrics.test.ts src/api/mock/generator.test.ts`：49 个测试通过 |
| 锁文件一致性 | `pnpm install --offline --frozen-lockfile --lockfile-only --ignore-scripts` 通过，锁文件无变化 |
| 生产构建 | 根路径及 `VITE_BASE=/board/` 构建通过 |
| 开发版浏览器 | Chrome / Playwright：五个页面 × 320、375、414、768、1440、1920px，共 30 组；图表像素、页面宽度、抽屉尺寸、滚动锁定、Escape、页签和事件翻条通过，控制台无应用错误或警告 |
| 生产版浏览器 | 五个页面 × 375、1440px 共 10 组相同交互检查通过 |
| 接口与深链接 | 拦截请求返回契约样本，在根路径和 `/board/` 下验证全部五个路由、分类下钻、事件抽屉刷新、数据源切换和读取失败状态 |
| CI 文件 | YAML 解析通过；独立 `webui` 任务运行 frozen install、check、build，不包含部署操作 |

真实 Canvas 检查在浏览器执行；jsdom 使用固定文字测量并跳过其不支持的伪元素参数，不据此声称验证了字体排版或浏览器布局。
浏览器脚本位于 `/tmp/webui-engineering-browser.mjs` 和 `/tmp/webui-engineering-api-browser.mjs`，截图为 `/tmp/webui-after-*.png`。

## 尚未验证的边界

> **2026-09-09 补记（这份报告是 09-06 的快照，不改写，只标注后续）：**
> 第 1 条和第 3 条已经不成立。只读 HTTP 服务已落地（`src/bin/webui.rs` ＋ `src/web/`），
> 12 条 `mysql_` 测试在隔离 MySQL 上验事务与只读取数；分页与聚合全部下推到后端
> （`/api/summary` · `/api/rooms` · `/api/agents` · `/api/categories` · `/api/events`），
> 前端已切过去，`dataset` 不再带事件明细。两份口径由
> `webui/src/domain/parity-vectors.json` 的金标向量对拍。第 2、4、5 条仍然成立。

1. 仓库尚无配套只读 HTTP 服务，本次 API 浏览器验证使用契约样本，未连接真实 MySQL / 原文服务。上线仍需后端联调和实际数据规模验证。
2. 元数据中的群日是否应存在，不能仅由缺失记录推断。当前完整性基于已有群日记录及既定窗口逻辑，未编造全部群与全部日期的笛卡尔积。
3. 保留现有一次装载元数据窗口、浏览器内筛选的接口方式。海量数据的分页 / 聚合下推需要后端接口设计，不在本次前端整理中新增协议。
4. AntD 公共包约 1.15 MB（gzip 366 KB），仍触发现有 900 KB 构建提醒。入口业务包由约 253 KB 降至约 146 KB，ECharts 由约 659 KB 降至约 603 KB；没有提高阈值隐藏提醒。
5. 未在 GitHub 远程运行新增 CI，也未部署 nginx 或生产资源；没有执行 commit / push。

## 修改基线

开始时 `git status --short` 只有 `?? webui/`，因此前端对比使用操作前源码快照，而不是将未跟踪文件视为可覆盖内容。
快照：`/tmp/chat2events-webui-before-engineering-20260906.tgz`，不含 `node_modules` 和 `dist`。
仓库级修改仅新增现有 CI 文件中的 WebUI 检查任务；Rust 实现、数据库和跑批配置未改动。
