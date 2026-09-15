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

在仓库根目录启动 Rust 只读后端：

```bash
cargo run --locked --bin webui -- /etc/chat2events <corpid>
```

默认监听 `127.0.0.1:8787`；第三个参数可指定监听地址。
前端通过 `/api/dataset` 一次读取同一 MySQL 快照中的词表、事件和群日记录。
默认仅加载最近七天，日期筛选会重新请求对应范围；群抽屉独立请求七天，事件深链接按 ID 补读。
缺记录与无法定位影响范围的后续失败都显示为未知，不能当作完整或零；该事件早于原文留存时返回 410。

前端只使用真实接口，探活与数据读取失败均显式报错。旧 `source` 参数不再切换数据源。

整体概览固定采用已确认工作台，入口为 `/overview`，统计最新七个自然日。
旧 `variant` 参数仅在入口清理，不再包含原型或多套页面实现。

## 本地排查

起不来、或页面报错时按这个顺序查：

```bash
pgrep -alf 'webui|vite'                                            # 进程在不在
lsof -nP -iTCP:8787 -iTCP:5273 -sTCP:LISTEN                        # 端口有没有人听
curl -s -o /dev/null -w '%{http_code}\n' 127.0.0.1:8787/api/meta   # 200 = 后端正常
curl -s 127.0.0.1:8787/api/dataset | python3 -m json.tool | head    # 数据长什么样
```

**进程在不等于它是当前代码。** 页面报「接口返回的数据不符合约定」而缺的正是某个新字段时，
先怀疑后端还跑着该字段加进去之前编译的二进制 —— `curl` 一下 `/api/dataset` 看键在不在，
比看进程列表准。重起：`pkill -f 'bin/webui'` 后再 `cargo run`。

后端启动即报缺列、或某列必须允许 NULL，是目标库没做表结构升级，升级 SQL 见
`../docs/deploy.md` 的「上线前的检查」。前端两处报错文案的区别：契约不符指字段形状对不上，
409 指数据本身（企业无数据、词表版本不一致）。

## 目录

```
src/
  domain/       领域契约与指标口径。**唯一能定义「一个数是什么意思」的地方**
    schemas.ts    zod 契约，字段名逐字对齐 schema.sql，类型由它推导
    metrics.ts    纯函数指标层，有测试
    definitions.ts 口径文案、待补数据清单、口径洞清单（界面文案的唯一来源）
  api/          只读接口客户端（全部 GET，响应过 zod）+ 数据装载
  app/          应用上下文、错误边界、全局与工作台布局
    theme/      固定浅色主题、自托管字体和语义令牌
                tokens.ts 保留外壳 / 公共图表视觉，workbench.ts 管理已确认页面主题
  components/   与业务无关的展示件（图表、指标条、状态、原语）
  features/     五个按需加载的视图 + URL 筛选状态
    overview/   概览入口、工作台、图表、消息汇总与回归测试
    insights/   事件、客服、追溯共享布局、指标条编排与页签工具栏
```

依赖方向：`features → components → app/theme`，`features → api → domain → lib`。
主题与布局不依赖页面实现；`domain` 和 `lib` 不依赖 React / DOM，可单独测试。
测试样本与指标对照实现放在 `src/test/mock/`，不参与生产构建。
路由资源或渲染失败由全局错误边界处理。

## 质量检查

`pnpm check` 执行 lint、格式、类型检查与全部前端测试；`pnpm build` 单独验证生产打包。
仓库 CI 的 `webui` 任务使用 Node.js 24、固定 pnpm 和 frozen lockfile 执行相同检查。
打 tag 时它会把 `dist/` 打成 `chat2events-webui-dist.tar.gz` 随 Release 发出来（根路径构建，`VITE_BASE` 默认 `/`）；**没有自动部署** —— 解包到静态目录仍由人执行。
jsdom 中仅补足 ECharts 文字测量和伪元素样式读取；真实布局、Canvas 与 Portal 行为需浏览器验证。

完整审核范围、清理清单、修复与验证边界见 [前端审核报告](AUDIT.md)。

## 部署

产出是纯静态文件，`/api/*` 反向代理到只读 JSON 服务，配置见 `deploy/nginx.conf`，
逐步 runbook 见 `../docs/deploy-webui.md`。
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
| `GET /api/metric/group?from=&to=` | `GroupDaily[]` | `b_merchant_group_metric_daily` |
| `GET /api/metric/agent?from=&to=` | `AgentDaily[]` | `b_merchant_group_agent_metric_daily` |
| `GET /api/failures?from=&to=` | `Failure[]` | `b_merchant_group_run_failure` |
| `GET /api/event/{id}/messages` | `Message[]` | `b_merchant_group_event.source_messages`，**未脱敏原文**。⚠️ 这一列有保留期（跟 raw 镜像的 `raw_retention_months` 同一个），过期后回 410「该事件早于原文留存」—— 那是留存期到了，不是故障 |
| `GET /api/summary` | KPI 一行 | 数据库算完只送数字，**不随窗口变大**。`sla_sec` 默认 1800，须与前端 `DEFAULT_SLA_SEC` 一致 |
| `GET /api/rooms` | 按群一行 | 只出「必须从明细算」的七项（含分位数）；`msg_count` / 每日序列 / 覆盖率天数继续用 `groupDaily` 拼 |
| `GET /api/agents` | 按客服一行 | **参与**（involved/rooms）与**首响归属**（owned/p50/p90/overdue）是两个口径，不能混。**没有 `unreplied`**：抽取保证「无平台回复 ⟹ agents 为空」，那一列结构上恒为 0；团队口径的无响应数在 `/api/summary` |
| `GET /api/events` | `{ rows, total, pages, truncated }` | `b_merchant_group_event`。延迟关联翻页（内层只碰覆盖索引、外层才回表）。`page` 1~200、`page_size` 1~100，越界 400 不截断；页码在护栏内但越过 `pages` 返回空 `rows`。**`total` 不按已知成功群日过滤**，与 `/api/summary` 的 `events` 不是一个集合，前端不做任何分页算术 |

四个聚合 / 明细接口收**同一组筛选参数**，全部进 SQL 的 `WHERE`：

| 参数 | 说明 |
|---|---|
| `from` / `to` | 日期窗口，不给用默认七天 |
| `room` / `agent` | 群号 / 客服 easyUserId |
| `types` | 逗号分隔的 `event_type`。**父类由前端展开成子类集合再传** —— 词表在前端手上，后端不再 join 一次 |
| `status` | `unreplied` / `replied` / `push` / `backlog`，与前端 `StatusFilter` 同名 |
| `overdue_only` | `true` / `false`，非法值 400 不静默当假 |
| `q` | **只匹配事件摘要**。搜索框还会命中群名 / 客服名 / 类型名，那些由前端解析成 id 走上面三个参数 |
| `sla_sec` | 超时线，默认 1800，须等于前端 `DEFAULT_SLA_SEC` |

⚠️ **前端页面尚未切过去**，`dataset` 仍是主路径。切换顺序见下面「待办」。
后端已与前端算法**逐个数字对拍通过**（5 群 × 7 项 ＋ 22 客服 × 9 项 ＋ 10 组筛选组合），
但那是在**新老并存**的前提下做的 —— 删掉 `dataset` 就没有基准可对了，所以要先切页面再删。

### 待办：前端切换的顺序

1. `useAnalytics` 的 `agg` 改读 `useSummary`
2. `RoomsPage` / `AgentsPage` 改读 `useRoomAggs` / `useAgentAggs`（与 `groupDaily` join 补消息级指标）
3. `OverviewCharts` 的按天序列改用 `summary.byDay`
4. `EventsPage` 改用 `useEventsPage` 服务端翻页
5. **最后**才把 `events` 从 `/api/dataset` 里去掉 —— 那时它只剩 `meta` ＋ `groupDaily`，
   1000 群 × 7 天是 7000 行，永远不会撞 `max_rows`，天花板随之消失

每切一步都拿同一份数据和老路径对拍，别攒到最后一起验。

字段形状以 `src/domain/schemas.ts` 为准，**每个响应都会被校验**：不符合约定就显式报错，
不静默渲染。理由见该文件顶部注释。
`meta.days` 必须是升序、唯一且有效的日期；时间字符串按 UTC+8 解析，不依赖浏览器时区。
事件时间顺序、归属日、主分类、首响归属和失败群日的 NULL 约束也在响应边界检查。

`/api/meta` 与 `/api/dataset` 的群元数据按 `(corp_id, official_room_id)` 左连接
`b_wecom_merchant_group`，历史群仍读取已删除配置。`group_name` 返回为 `rooms[].alias`，
名称非空时 `rooms[].alias_is_authoritative` 为 `true`；未匹配或名称为空时继续标记待补。
`rooms[].merchant_id` 是可空字符串，保留 BIGINT 精度，暂不用于商家展示或筛选。
客服姓名仍由顶层 `alias_is_authoritative` 控制。工作台 MySQL 账号需具备该配置表的 SELECT 权限。

## 三条不能破的口径

1. **首响、无响应、超时的分母是商家发起事件数**，不是事件总数。平台发起的工单推送
   首响恒 0 秒且永远算已回复，混进分母会让比率静默偏低。
2. **分位数一律在事件明细上现算**，绝不对每日 p50 取平均：分位数不可加。
3. **null 表示「没算出来」，一路保持 null**，界面显示长横线，绝不显示 0。
   抽取失败的「群 × 日」在 `metric_daily` 上是 NULL，在 `agent_metric_daily` 上是整行缺失。

这三条在 `src/domain/metrics.test.ts` 里逐条钉住。改实现可以，改期望值必须先改口径。

## 待补齐的数据能力

库里没有来源的能力在界面上一律**原地标注**，不编造，也不另设清单面板：
客服回复消息数（含日粒度参与）与客服姓名标在客服页的「数据边界」与姓名标记上，
工作日历标在时长指标的 ⓘ 里。补齐各需要什么见 `design.md`。
