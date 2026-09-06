# 群聊事件与客服效率分析工作台

面向客服主管与其上级的**只读**分析看板。它与跑批解耦：只从 MySQL 和摄取端口取数，
**不写任何表、不调模型、不参与跑批**，跑批不知道它存在。

## 起步

使用 Node.js 24 和 pnpm 11.21.0（`package.json` 固定包管理器版本）。
Node.js 最低兼容范围见 `engines`；Node.js 20 无法运行当前 Vitest / ESLint。

```bash
pnpm install --frozen-lockfile
pnpm dev        # http://localhost:5273，/api 转发到 VITE_API_PROXY（默认 127.0.0.1:8787）
pnpm check      # lint + format + 类型 + 单元测试，提交前必须干净
pnpm build      # 产出 dist/，纯静态文件
```

默认模式仅在 `/api/meta` 探活网络不可达、超时、404 或 5xx 时回落到模拟数据源，顶栏常驻「模拟数据」标记。
`?source=api` 强制只走真接口（失败即报错，不回落），`?source=mock` 强制模拟。
探活的权限 / 契约错误，以及探活成功后的数据读取错误，均显式报错。
概览、筛选重置、下钻和数据源切换保留明确的 `source` 选择；数据集与消息缓存按数据源隔离。

整体概览固定采用已确认工作台，入口为 `/overview`，统计最新七个自然日。
旧 `variant` 参数仅在入口清理，不再包含原型或多套页面实现。

## 目录

```
src/
  domain/       领域契约与指标口径。**唯一能定义「一个数是什么意思」的地方**
    schemas.ts    zod 契约，字段名逐字对齐 schema.sql，类型由它推导
    metrics.ts    纯函数指标层，有测试
    definitions.ts 口径文案、待补数据清单、口径洞清单（界面文案的唯一来源）
  api/          只读接口客户端（全部 GET，响应过 zod）+ 数据源仲裁 + 模拟数据源
  app/          应用上下文、错误边界、全局与工作台布局
    theme/      固定浅色主题、自托管字体和语义令牌
                tokens.ts 保留外壳 / 公共图表视觉，workbench.ts 管理已确认页面主题
  components/   与业务无关的展示件（图表、指标条、状态、原语）
  features/     五个按需加载的视图 + URL 筛选状态
    overview/   概览入口、工作台、图表、消息汇总与回归测试
    insights/   事件、客服、追溯共享布局与指标条
```

依赖方向：`features → components → app/theme`，`features → api → domain → lib`。
主题与布局不依赖页面实现；`domain` 和 `lib` 不依赖 React / DOM，可单独测试。
模拟数据生成器仅在需要模拟源时动态加载，路由资源或渲染失败由全局错误边界处理。

## 质量检查

`pnpm check` 执行 lint、格式、类型检查与全部前端测试；`pnpm build` 单独验证生产打包。
仓库 CI 的 `webui` 任务使用 Node.js 24、固定 pnpm 和 frozen lockfile 执行相同检查，无自动部署。
jsdom 中仅补足 ECharts 文字测量和伪元素样式读取；真实布局、Canvas 与 Portal 行为需浏览器验证。

完整审核范围、清理清单、修复与验证边界见 [前端审核报告](AUDIT.md)。

## 部署

产出是纯静态文件，`/api/*` 反向代理到只读 JSON 服务，示例见 `deploy/nginx.conf`。
两条必须做到：`try_files` 回落 `index.html`（前端走 BrowserRouter，深链接要能刷新），
`index.html` 不许缓存（带 hash 的资源可以长缓存，入口不行）。

子路径部署设 `VITE_BASE=/board/` 再 build。
`/api` 始终位于站点根路径，不随 `VITE_BASE` 改变。
根路径部署可直接参考 nginx 示例；子路径部署需将静态文件放到 `/board/` 对应目录，
并将 SPA 回退、入口缓存与资源缓存规则同时调整为 `/board/index.html` 和 `/board/assets/`。

## 接口契约

| 路径 | 返回 | 对应表 |
|---|---|---|
| `GET /api/meta` | 语料窗口、群与客服名册、词表 | — |
| `GET /api/events?from=&to=` | `Event[]` | `b_merchant_group_event` |
| `GET /api/metric/group?from=&to=` | `GroupDaily[]` | `b_merchant_group_metric_daily` |
| `GET /api/metric/agent?from=&to=` | `AgentDaily[]` | `b_merchant_group_agent_metric_daily` |
| `GET /api/failures?from=&to=` | `Failure[]` | `b_merchant_group_run_failure` |
| `GET /api/event/{id}/messages` | `Message[]` | 摄取端口 `read_by_ids`，原文 |

字段形状以 `src/domain/schemas.ts` 为准，**每个响应都会被校验**：不符合约定就显式报错，
不静默渲染。理由见该文件顶部注释。
`meta.days` 必须是升序、唯一且有效的日期；时间字符串按 UTC+8 解析，不依赖浏览器时区。
事件时间顺序、归属日、主分类、首响归属和失败群日的 NULL 约束也在响应边界检查。

`/api/meta` 若能提供真实的群名与客服姓名，请一并把 `alias_is_authoritative` 置 `true`，
界面会自动去掉「别名 待补」标记。

## 三条不能破的口径

1. **首响、无响应、超时的分母是商家发起事件数**，不是事件总数。平台发起的工单推送
   首响恒 0 秒且永远算已回复，混进分母会让比率静默偏低。
2. **分位数一律在事件明细上现算**，绝不对每日 p50 取平均：分位数不可加。
3. **null 表示「没算出来」，一路保持 null**，界面显示长横线，绝不显示 0。
   抽取失败的「群 × 日」在 `metric_daily` 上是 NULL，在 `agent_metric_daily` 上是整行缺失。

这三条在 `src/domain/metrics.test.ts` 里逐条钉住。改实现可以，改期望值必须先改口径。

## 待补齐的数据能力

库里没有来源的能力在界面上一律标注，不编造。完整清单见页尾，或
`src/domain/definitions.ts` 的 `DATA_GAPS`：已解决 / 后续回复时效 / 客服回复消息数 /
客服姓名与群名 / 工作时间口径 / 客服当天实际参与。
