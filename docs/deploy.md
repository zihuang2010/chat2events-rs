# 部署（Linux）

跑批是 **T+2 定时任务，跑完即退出**。只读工作台由独立的 `webui` 进程承接 HTTP，
下面分别说明二进制、定时跑批与只读旁路的启动方式。

---

## 一句话结论：Linux 不需要装 DuckDB

`Cargo.toml` 里 `duckdb = { version = "...", features = ["bundled"] }` ——
DuckDB 的 C++ 源码在**编译期**静态链进二进制，运行时没有 `libduckdb.so` 依赖。

TLS 同理：`sqlx` 走 `tls-rustls-aws-lc-rs`，`reqwest` 走 rustls
（`Cargo.lock` 里只有 `openssl-probe`，**没有 `openssl-sys`**），所以目标机
也不需要 OpenSSL 开发包。

**目标机的运行时依赖只有两样**：

| 依赖 | 为什么 |
|---|---|
| glibc | 二进制是动态链接的 gnu target（见下面「glibc 版本」） |
| `ca-certificates` | `rustls-platform-verifier` → `rustls-native-certs` 要读 `/etc/ssl/certs` 才能验 OSS / 模型端点 / MySQL 的证书 |

---

## 编译

### ⚠️ macOS 上编不出 Linux 二进制

开发机是 macOS，`cargo build --release` 出的是 Mach-O，扔到 Linux 上不能跑。
而 bundled DuckDB 是 **C++**，交叉编译要配一套 `x86_64-unknown-linux-gnu` 的
g++ 工具链 —— 三条路里最疼的一条。

**正常路子是从 CI 的 Release 里下产物**（见下面「glibc 版本」）——
它在 manylinux2014 容器里编，下界压到 glibc 2.17，有 objdump 断言兜着。

要在机器上自己编的话，前提是**那台机器的 g++ 够新**。当前目标机是
CentOS 7 / gcc 4.8.5 —— C++11 支持不完整，**编不动 bundled DuckDB**，
这条路在它上面直接堵死。下面这段只适用于 g++ ≥ 8 的构建机：

```bash
# 一次性准备
apt install -y build-essential ca-certificates   # 必须有 g++ ≥ 8，DuckDB 是 C++
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

rustc -V    # 必须 ≥ 1.85：本仓库是 edition 2024
```

```bash
# 构建
cargo build --release
# 产物（六个二进制，CI 的 release job 打成一个 tar 包）：
# target/release/{chat2events-rs, webui, backfill, recompute, recover, taxonomy}
```

⚠️ **首次编译会现场编 DuckDB 的 amalgamation**：慢（分钟级到十几分钟）、吃内存
（给 ≥ 4 GB，否则 g++ 会被 OOM killer 干掉，表现为莫名其妙的 `signal: 9`）。
之后增量编译不再碰它。

### glibc 版本

规矩是**目标机 glibc ≥ 构建机 glibc**，反过来会在启动时报 `GLIBC_2.xx not found`
（C++ 那半边同理，报 `GLIBCXX_3.4.xx not found` —— DuckDB 是 C++）。
查法：目标机上 `ldd --version`。同机构建不存在这个问题。

**目标机是 CentOS 7：glibc 2.17，libstdc++ 来自 GCC 4.8（最高 `GLIBCXX_3.4.19`）。**
而 GitHub runner 的 `ubuntu-latest` 是 glibc 2.39 —— 直接在 runner 上
`cargo build --release`，产物到机器上一行都跑不了。所以 CI 的 release job
**只把编译这一步丢进 `quay.io/pypa/manylinux2014_x86_64` 容器**
（CentOS 7 底 + devtoolset-10 的 gcc 10，DuckDB 用 4.8 编不动），并且
`-static-libstdc++ -static-libgcc` 把新 gcc 的 C++ 符号静态链进去。

> 为什么不用 `jobs.<id>.container` 把整个 job 塞进这个镜像：GitHub Actions 的
> node20 runtime 要 glibc ≥ 2.28，CentOS 7 里 `checkout` / `rust-toolchain`
> 全部起不来。job 照常跑在 ubuntu-latest，容器只包住 `cargo build`。

产物实际要求的符号版本由 CI 里的「校验 glibc / libstdc++ 下界」一步断言
（`objdump -T`，最高符号超过 `GLIBC_2.17` 或 `GLIBCXX_3.4.19` 就 fail），同时作为
`glibc-baseline.txt` 随 Release 发出来。换镜像、换 `RUSTFLAGS` 之后二进制悄悄
要上新 glibc，这条会当场拦住，不必等部署到机器上才发现。

> 明确没做：`cross` 交叉编译、musl 全静态、release job 的构建缓存。
> 前两条 bundled DuckDB（C++）都要另配一套工具链，容器已经解决问题；
> 第三条是因为 `CARGO_HOME` 在容器里，跨不过 `rust-cache` 的边界 ——
> release 只在打 tag 时跑，多花的十几分钟不值得为它维护一套缓存。

---

## 机器上放什么

```text
/opt/chat2events/
    chat2events-rs              # 每日跑批，来自 release tar 包
    webui                       # 只读工作台（独立启动，可选）
    backfill                    # 人工：补跑历史窗口（⚠️ 写穿冻结区）
    recompute                   # 人工：词表升版后重打标
    recover                     # 人工：补齐未完成分类
    taxonomy                    # 人工：看语料 / 试打 / 转 SQL
/etc/chat2events/
    config.toml                 # 调参与端点，跟仓库里那份同源
    secrets.toml                # 0600，config.rs 起手就检查权限，不对直接崩
/var/lib/chat2events/
    data/raw/                   # OSS 月文件的本地镜像，需写权限
```

跑法（`main.rs` 的两种）：

```bash
./chat2events-rs                     # 读 **当前目录** 的 config.toml / secrets.toml
./chat2events-rs /etc/chat2events    # 生产：从指定目录读
```

命令行参数是**配置目录**，不是配置文件。

### ⚠️ `raw_root` 是相对 cwd 的，不是相对配置目录

`config.toml` 里 `raw_root = "./data/raw"` 直接当路径用（`config.rs` 不做任何
相对配置目录的重定位）。所以生产上二选一：

* 把 `raw_root` 改成绝对路径 `/var/lib/chat2events/data/raw`，或
* 在 systemd unit 里锁死 `WorkingDirectory=/var/lib/chat2events`。

两个都不做的话，raw 区会跟着 cwd 漂移，换个地方跑就等于**全量重新下载**。

### 磁盘

`raw_retention_months = 2` ⇒ 上界约 2 个月。按 1000 群 × 500 条/天 × 1.2 KB
估算 **≈ 36 GB**（每天新增约 600 MB）。清理在每轮拉取后自动执行，
保留起点锚在窗口上，调大 `lookback_days` 不会误删本轮要读的月份。

⚠️ 这个目录**不再是 webUI 下钻的可见范围** —— 原文渲染快照在抽取时落进
`b_merchant_group_event.source_messages`，工作台不读文件。这里的保留期只决定
「抽取失败重跑 / backfill 补跑还读不读得到原文」。

### ⚠️ raw 区是未脱敏的客户正文，权限跟 `secrets.toml` 同级

这 36 GB 里有客户手机号、门牌号级住址、真实姓名（实测 1850 条：193 / 88 / 101）。
`mirror` 现在**建目录 0700、建文件 0600**，不再依赖部署时的 umask。
历史 `<cache_dir>/*-nearest.json` 中的姓名片段可能可读回来，仍应按 0600 保管。
当前实现不再读取或生成类心文件，也不会自动删除历史文件。

⚠️ **`mode()` 只在创建时生效，已经落地的旧文件保持原权限。**
从 0.1.5 之前的版本升上来时，在目标机上执行一次：

```bash
chmod -R go-rwx /var/lib/chat2events/data/raw
find /var/lib/chat2events -name '*-nearest.json' -exec chmod 600 {} +
```

### 分类实现升级

- 删除 `classify.margin` 配置；`recompute` 用法为 `<config_dir> <version> <since> <until> [并发度]`，不再接受种子数。
- 先前版本缓存使用 `<version>-<64位策略指纹>.ndjson`，本次持久索引升级见下文。旧短指纹缓存无法确认模型身份，不自动迁移；
  首轮可能重新请求大模型，应预留调用额度。已有数据库事实和旧缓存文件不会自动改写或删除。
- 同一策略缓存只允许一个进程写入，日常跑批、重打标、词表试打需错开运行；占用时启动直接报错。
- 模型和词表策略变更需按既有升版流程协调日常跑批与历史重打标，避免新旧口径混用。

### 出网白名单

| 目标 | 用途 |
|---|---|
| `https://jdd-rh-wechat.oss-cn-zhangjiakou.aliyuncs.com` | V4 签名读取私有 OSS 月文件（`ingest.oss`） |
| `https://dashscope.aliyuncs.com` | ③ 抽取和 ⑤ 打标的模型端点（`llm.extract.base_url` / `llm.classify.base_url`）。⚠️ **两队现在是两个独立配置**，换成不同端点时这张表要各占一行 |
| MySQL | 双向：读索引表 `b_wecom_group_message_month_file`，写 ⑦ 的三张表 + `run_failure` |

**不需要** `https://extensions.duckdb.org` —— `read_json_auto` 所在的 json 扩展已经
静态链进二进制（`Cargo.toml` 的 `duckdb` features 里那个 `json`）。把它去掉的话，DuckDB
会在第一次 `read_room()` 时按 AUTOINSTALL 默认值联网下载，内网出不去 = 整轮死。CI 的
「校验 json 扩展已静态链」那步就是挡这个的。

⚠️ **踩过一次（2026-09-02）**：`ingest` 的 SQL 曾用 `AT TIME ZONE 'Asia/Shanghai'`，
而这是 **ICU 扩展**提供的，ICU 没法像 json 那样静态链（`duckdb` 的 `icu` feature 会把
整个构建切成 `bundled-cmake`）。CI 在公网跑、平台串是真的 → 自动下到、全绿；
目标机在内网、平台串又是 `DUCKDB_CUSTOM_PLATFORM` 那个假的 `linux_amd64_gcc4`
（`extensions.duckdb.org` 上没有这个平台 → HTTP 404）→ **每个群都在 `read_room()` 第一句失败**。
现已改成 core 的 `make_timestamp(... + 8h)`（Asia/Shanghai 自 1991 年无夏令时，恒 UTC+8），
并在建 DuckDB 实例时 `SET autoinstall_known_extensions = false` —— 于是「SQL 又需要某个
扩展」这件事在干净机器（CI runner）上当场报错，不用等部署。

### 私有 OSS 配置

`config.toml` 的 `ingest.oss` 使用服务端点 `https://oss-cn-zhangjiakou.aliyuncs.com`、
bucket `jdd-rh-wechat`、签名地域 `cn-zhangjiakou`。客户端自动加上 bucket 子域名；
不要把完整的 bucket 域名再次填进 `endpoint`。旧 `ingest.download_base_url` 已移除。

在现有 `secrets.toml` 中新增以下节，替换占位值，并保持文件权限为 `0600`：

```toml
[oss]
access_key_id = "填写 RAM 用户的 AccessKey ID"
access_key_secret = "填写 RAM 用户的 AccessKey Secret"
```

RAM 用户需具备月文件对象路径的 `oss:GetObject` 权限，资源为
`acs:oss:*:*:jdd-rh-wechat/<月文件前缀>/*`，前缀以索引表的 `ndjson_object_key` 为准。
对象列表来自 MySQL，不需要 `oss:ListObjects`。bucket 继续保持私有。
每次 GET（包括重试）都生成 V4 请求头签名，URL 不携带密钥或签名；生产机需保持时钟同步。
403 按群级失败处理，不会退回匿名下载。首次部署先用一个已知月文件验证读取权限。

协议依据：[GetObject](https://www.alibabacloud.com/help/zh/oss/developer-reference/getobject)、
[V4 请求头签名](https://help.aliyun.com/zh/oss/developer-reference/recommend-to-use-signature-version-4/)。

---

## 定时跑

日常窗口跳过当天和昨天，读取 `[T-(N+1), T-2]`，`N = ingest.lookback_days`。
当前 N=2：9 月 7 日处理 9 月 4 日、5 日，9 月 8 日处理 9 月 5 日、6 日。
`run_date` 仍记录实际运行日；人工 `backfill` 的显式起止日期保持原义。
切换规则不会删除旧规则已写入的窗口外日期，网页仍以数据库最新日期为展示终点。

日志**全部走 stderr**，非 tty 时自动关掉 ANSI 颜色 —— 直接进 journald 就是干净的。
**stdout 全程不写一个字节**，结果只落 MySQL。

```ini
# /etc/systemd/system/chat2events.service
[Unit]
Description=chat2events T+2 batch
After=network-online.target

[Service]
Type=oneshot
User=chat2events
WorkingDirectory=/var/lib/chat2events
ExecStart=/opt/chat2events/chat2events-rs /etc/chat2events
```

```ini
# /etc/systemd/system/chat2events.timer
[Unit]
Description=chat2events daily

[Timer]
OnCalendar=*-*-* 03:15:00
Persistent=true

[Install]
WantedBy=timers.target
```

```bash
systemctl enable --now chat2events.timer
systemctl start chat2events.service      # 手动跑一轮
journalctl -u chat2events -f             # 看日志
```

### ⚠️ 别把非零退出码吃掉

`daily.round_deadline_secs`（默认 21600 = 6 小时）到点后不再启动新的下载 / 新的群，
在飞的跑完就收工，然后**以非零码退出** —— 本轮没跑完必须看得见。所以：

* 不要在 unit 里加 `SuccessExitStatus=` 把它抹平；
* 告警接在这个退出码上，不要接在日志关键词上。

单群失败不会中止其余群；全部在飞任务收尾后，只要存在 failed 或 unsynced，进程就以非零码退出。
预算耗尽未开始的群也导致非零码。失败明细看 `run_failure`；落库与记账都失败时，以日志为准。

### 临时排障

`RUST_LOG` 存在就盖过 `config.toml` 的 `log.level`，不用改文件：

```bash
RUST_LOG=chat2events_rs=debug ./chat2events-rs /etc/chat2events
```

⚠️ 调 `debug` 不会让 async-openai 更啰嗦（它只埋了 warn 级别的点：429 限流 header、
retry-after、5xx），只会让 reqwest/hyper 变吵。

---

## 上线前的检查

### 失败影响面升级

停止所有跑批、补标与重打标，备份后执行；新库直接使用 `schema.sql`。

```sql
ALTER TABLE b_merchant_group_run_failure
    ADD COLUMN window_since DATE NULL
        COMMENT '本次失败覆盖的数据窗口起点；NULL=升级前的历史行，按影响全历史保守处理'
        AFTER stage,
    ADD COLUMN window_until DATE NULL
        COMMENT '本次失败覆盖的数据窗口终点' AFTER window_since;
```

**不要回填历史行。** `run_date` 是跑批日不是数据日，T+2 之下两者差两天，而
`lookback_days` 可配、backfill 窗口任意 —— 从跑批日反推数据窗口不可靠，
拿它回填等于用猜的影响面去放行本该存疑的天。历史行保持 NULL，只读工作台
继续按「影响全历史」处理它们（承重不变量 5：历史未知不能推断成已知）。

升级之后新写入的失败行都带窗口，只读工作台判事实新鲜度时把失败夹在它自己那个
窗口里：一次拉取失败或「没轮到」不再让这个群**全部历史**掉出聚合分母 ——
此前那些天含冻结区里早已成功的日子，而冻结区不会再被重抽，unknown 是永久的。

### 只读工作台响应缓存

白天的查询走内存缓存（`src/web/cache.rs`），失效靠库里的「数据戳」：每个请求先读
四张表各自的最后一次写，戳变了整个缓存作废；戳距现在不足 60 秒视作跑批还在写，只查不存。
跑批不需要知道缓存存在。戳查询要走索引，已有库补这两条（新库直接用 `schema.sql`）：

```sql
ALTER TABLE b_merchant_group_event ADD KEY idx_modified (gmt_modified_time);
ALTER TABLE b_merchant_group_metric_daily ADD KEY idx_modified (gmt_modified_time);
```

没有它们缓存照样工作，但每个请求的戳查询本身就是两次全表扫描。
`[web]` 新增 `cache_bytes`（0 = 关）；`concurrency` 抬到 48 —— 一页 7 个请求、不排队，
12 只够一个人。响应头 `X-Cache: HIT / MISS / BYPASS` 可以直接看命中情况。
⚠️ 群名表 `b_wecom_merchant_group` 不在戳里（别人的表，没有可用时间列），群改名要重启 `webui` 或等下一次跑批。

### 原文快照的保留期

`b_merchant_group_event.source_messages` 是**未脱敏的客户正文**（实测 1850 条里 193 条带
手机号、88 条带门牌址、101 处真名），而只读工作台无登录。它跟 raw 镜像同一个保留期
（`ingest.raw_retention_months`），由跑批在清理月目录之后顺手置空 ——
清的是**展示列**不是事实列，不碰冻结（承重不变量 1 只管事实列）。过期后
`/api/event/{id}/messages` 回 410「该事件早于原文留存」，那是留存期到了不是故障。

⚠️ **每轮只清刚滑出保留期的那一个月**。`occurred_on < 边界` 配上
`source_messages IS NOT NULL` 只能回表才判得了，稳态下那些行早清完了 ——
不夹下界就是每晚白扫整段历史、一行都清不到，而那个代价只跟「库里攒了多久」有关。

**代价**：跑批连续多天没跑会漏掉中间的月份。追平跑一次（`<边界>` 取
「窗口起点往前推 `raw_retention_months - 1` 个月」那个月的 1 号）：

```sql
-- 分批跑，别一条语句扫全表；重复执行直到 affected rows 为 0
UPDATE b_merchant_group_event SET source_messages = NULL
WHERE occurred_on < '<边界>' AND source_messages IS NOT NULL
LIMIT 500;
```

### 只读接口的压缩

`webui/deploy/nginx.conf` 里 `gzip on` **不压缩反代响应** —— `gzip_proxied` 默认是 `off`，
所以 `/api/*` 一直是未压缩出网，而群日记录那种「几千行同一组键」的 JSON 正是压缩比最高的形状。
示例里已加 `gzip_proxied any;`，**目标机上的配置同步由人执行**。

「改了没生效」在这件事上完全静默：响应照常返回，只是大八倍。上线后跑一次：

```bash
curl -s -o /dev/null -D - -H 'Accept-Encoding: gzip' \
  'http://chat2events-board.internal/api/summary' | grep -i content-encoding
# 期望：content-encoding: gzip   —— 没有这一行就是没生效
```

⚠️ 响应体小于 `gzip_min_length`（1024 字节）时本来就不压缩，验证要挑一个够大的窗口
（`/api/dataset?from=...&to=...` 最稳）。**不加任何 Rust 依赖**：压缩是反代的事。

### 事实新鲜度与查询预算升级

停止所有跑批、补标与重打标，备份后执行；新库直接使用 `schema.sql`。
若尚未升级独立打标流水线，先完成下一节，再执行本节。

```sql
ALTER TABLE b_merchant_group_metric_daily
    ADD COLUMN fact_completed_time DATETIME(6) NULL
        COMMENT '事实成功保存时间；仅成功抽取写入，打标不修改；NULL表示缺少可信完成凭据'
        AFTER agent_accounts,
    ADD KEY idx_corp_day (corpid, dt);

-- ⚠️ 只读工作台的概览要的是**覆盖索引**，不是 (corpid, occurred_on) 两列。
-- 早先版本这里加的是 idx_corp_day，它是下面这条的前缀、已被取代：
-- 已经执行过旧版本的库先 DROP KEY idx_corp_day，新库直接用 schema.sql。
ALTER TABLE b_merchant_group_event
    ADD KEY idx_overview (corpid, occurred_on, roomid, asker_role, event_type,
                          first_msg_time, first_agent_reply_time, last_msg_time);

ALTER TABLE b_merchant_group_run_failure
    MODIFY COLUMN gmt_created_time DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        COMMENT '创建时间；与事实完成凭据同精度，避免同秒失败被遗漏',
    MODIFY COLUMN gmt_modified_time DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        ON UPDATE CURRENT_TIMESTAMP(6) COMMENT '更新时间',
    ADD KEY idx_room_stage_time (corpid, roomid, stage, gmt_created_time);
```

历史 `fact_completed_time` 保留 NULL，工作台显示新鲜度未知；不要用通用修改时间回填，
因为它可能来自标签更新。之后成功保存事实会写入可信时间；重打标、补标都不会推进它。
冻结区不为回填凭据而重新抽取。旧失败记录秒级时间无法还原微秒，新记录与事实凭据保持同精度。

在 `[llm]` 添加 `request_timeout_secs = 1200`，包含 SDK 重试、Retry-After 等待和分类重发。
它是一次逻辑调用的总预算；单次响应超时仍由 `timeout_secs` 管理。总预算耗尽报失败，不切段。

将仓库 `config.toml` 的 `[web]` 节加入只读配置：请求并发、原文扫描并发、结果字节、行数与查询超时均必填。
超限响应为 413，名额耗尽为 503，查询超时为 504，均不返回残缺统计。原文扫描开始后不因 HTTP 取消而提前释放扫描名额。
只读连接池同步设置 MySQL `max_execution_time`，数据库端也会终止超时 SELECT；业务时区仍固定为 +08:00。
只读配置只需 `[mysql]`、`[log]`、`[web]`；只读 `secrets.toml` 只需 `[mysql].url`，无需模型或 OSS 凭据。
**不需要 `[ingest].raw_root`** —— 原文下钻读 `b_merchant_group_event.source_messages`，只读工作台一个文件都不读。

当前硬上限：每个下载增量 64 MiB；单群会话与单群事件的字段预算各 32 MiB；
DuckDB 每进程工作集 512 MB、4 个线程、临时目录最多 1 GB。
这些是保守起点，不是 RSS 承诺：还要计入容器、原生查询结果、字符串分配与两个独立进程。
合法大月文件超过下载预算会显式失败；处理它需实现固定缓冲临时落盘，不能用扩大并发解决。

### 分类缓存与人工恢复

分类缓存升级为 `<version>-<64位策略指纹>.sqlite`，通过主键读盘；SQLite 内嵌在程序内，无需安装或启动数据库进程。
首次打开按行导入同名 `.ndjson`，答案与导入标记同事务提交，中断后可重试。
重复 hash 保留第一条合法答案；坏行与未完成尾行不导入，原文件保留作升级前备份及互斥锁载体。
SQLite 页缓存目标为 2 MiB，禁止 mmap；新答案事务持久化后才返回，写入失败后本进程停止提交。
不要删除 SQLite 文件来“清缓存”，也不要在升级后直接切回只读 NDJSON 的旧程序；备份时停止写入并同时保留两种文件。
短指纹旧缓存仍无法确认策略身份，不自动合并。

人工补齐未完成分类：

```bash
/opt/chat2events/recover /etc/chat2events 2026-08-01 2026-09-05
# 源码树里：cargo run --locked --bin recover -- /etc/chat2events 2026-08-01 2026-09-05
```

先停止覆盖相同群日的日常跑批和重打标，再执行恢复。按群日状态选择抽取成功但分类 pending/failed 的项，
包括零事件；只补缺失标签，已完成答案保持，整群日成功后才发布指标。数据库与缓存已有答案冲突、缓存缺失或版本不符时显式失败，
需要先核对模型策略并恢复原缓存备份；不会把旧模型的答案灌入新策略缓存。词表升版用 `recompute`。冻结事实及其完成凭据均不修改。
失败隔离到群日，其余项继续，最终有失败则非零退出；自动跑批仍不自动扫描历史待办。

观测分类批次日志中的 `queue_wait_ms`、`classify_ms`；跑批汇总的 `projected_extract_hours`
只外推抽取，不代表包含分类排空的整轮耗时。

2026-09-07 开发机合成容量检查（Rust debug 测试进程，不是生产承诺）：
分类缓存 2,000 条的峰值 RSS 为 32,784,384 字节，100,000 条为 35,422,208 字节；后者重新打开并查三条答案约 1 ms。
MySQL 8.4 的 1,000 行、10 企业样本中，日期查询使用企业日期索引，估计读 1 行；群失败查询估计读 10 行。
日期元数据查询的中间结果由 200 行降为 2 行；实际时延受机器、数据分布和缓存影响，不据此承诺生产加速倍数。

### 独立打标流水线升级

先停止旧跑批，执行以下一次性表结构变更，再同时更新跑批、只读后端与前端。
新库直接执行根目录 `schema.sql`，不重复执行下面的 ALTER。

```sql
ALTER TABLE b_merchant_group_event
    MODIFY COLUMN event_type VARCHAR(64) NULL COMMENT '主类；NULL=尚未完成打标',
    MODIFY COLUMN event_types JSON NULL COMMENT '标签全集；NULL=尚未完成打标',
    MODIFY COLUMN taxonomy_version VARCHAR(16) NULL COMMENT '词表版本；NULL=尚未完成打标';

ALTER TABLE b_merchant_group_metric_daily
    ADD COLUMN classification_status ENUM('pending','ok','failed') NOT NULL DEFAULT 'ok'
        COMMENT '打标状态；pending=事实已保存，ok=标签与分类指标齐全，failed=打标未完成'
        AFTER extraction_status,
    ADD COLUMN agent_accounts JSON NULL COMMENT '本轮群窗口的平台客服账号映射'
        AFTER classification_status;

ALTER TABLE b_merchant_group_run_failure
    ADD COLUMN stage ENUM('extract','classify') NOT NULL DEFAULT 'extract'
        COMMENT '失败阶段；打标失败不使事实失效' AFTER reason;
```

在 `[classify]` 中新增 `concurrency = 6`，`[ingest] room_concurrency` 设为 `10`。
前者限制所有群合计的打标批次，后者限制读取、抽取与事实保存。

⚠️ **`[llm]` 已拆成两个模型**：共用键（`reasoning_effort` / `temperature` /
`timeout_secs` / `connect_timeout_secs` / `request_timeout_secs`）留在 `[llm]` 节头下，两队各自的
`model` / `base_url` / `max_tokens` 分别在 `[llm.extract]` 和 `[llm.classify]` 子节里。
**共用键必须写在两个子节的节头之前**，否则会静默变成子节字段。
`[llm.classify].max_tokens` 必须小于 `[llm.extract].max_tokens`，启动期有断言拦着 ——
它拦的是「把 `[llm.extract]` 整段抄过去只改 model」这一种错法。
`secrets.toml` 的 `[llm].api_key` **不拆**，两队共用同一个 key。

⚠️ **`[mysql].max_connections` 必须 ≥ `room_concurrency` + `classify.concurrency`**
（现在是 10 + 6 = 16，配的是 24）。此前那条规则写的是「≥ room_concurrency」，
那是打标队列独立出来之前的形状，只算了两个消费者里的一个。
新字段缺失或标签列未改为可空时，启动检查直接报错。历史失败记录按抽取阶段处理，保留原来“不发布新事实”的含义。

事件保存与标签更新独立提交。正常收尾排空打标队列；进程被中断时内存 channel 会丢失未处理任务，
本版不自动跨重启补标。窗口外未完成分类用上述 `recover` 人工恢复；重跑当前窗口仍会重新抽取并安排打标。
日志分别汇总 `extracted`、`classified` 与 `classify_failed`，任一阶段失败都返回非零退出码。

### 首响口径改为工作时段

首响时效与超时从**墙钟差**改为**工作时段 `[08:30, 21:00)`**，与 `followup_wait_max_sec`
统一。夜里 23:00 进来、次日 09:00 回的消息，此前算 10 小时，现在算 30 分钟。
没有工作日历，**周末与节假日照常算工作日**。

口径定义在 `src/worktime.rs` 一处，Rust（⑥ 写 `first_reply_p*_sec`）、
SQL（webUI 查询期现算）两份都从那里的常量拼出来；前端第三份在
`webui/src/domain/worktime.ts`，三个常量必须与 Rust 逐字相同。

**要不要跑这段迁移**取决于你在不在乎历史。不跑的话，
`b_merchant_group_metric_daily.first_reply_p50_sec / p90_sec` 上
**上线前的行是墙钟差、之后的行是工作时段秒数**，同一列两种含义、没有任何标记，
而工作时段口径只会让数字变小 —— 正是「偏小但看起来正常」那一类。**建议跑。**

⚠️ 只影响 `b_merchant_group_metric_daily` 那两列。webUI 的首响、超时、分位数全是查询期
从 `b_merchant_group_event` 的两个时间列现算的，**换了代码就自动是新口径，不需要迁移**。

```sql
-- 三个常量取自 src/worktime.rs：WORK_DAY_SEC=45000、WORK_OPEN_SEC=30600（08:30）、
-- WORK_CLOSE_SEC=75600（21:00）。改过那边就要同步改这里。
-- 分位数定义逐字复刻 web/query.rs 的 grouped_quantiles：
--   1-based 行号 LEAST(n, FLOOR(n * p) + 1)，取原始值不插值。
-- 事实列一个字节不动，只重算指标列 —— 指标表不受分片冻结约束（承重不变量 1 管的是事实列）。
UPDATE b_merchant_group_metric_daily m
JOIN (
    SELECT corpid, roomid, occurred_on,
           MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.5) + 1) THEN secs END) AS p50,
           MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.9) + 1) THEN secs END) AS p90
    FROM (
        SELECT corpid, roomid, occurred_on, secs,
               ROW_NUMBER() OVER (PARTITION BY corpid, roomid, occurred_on ORDER BY secs) AS rn,
               COUNT(*)     OVER (PARTITION BY corpid, roomid, occurred_on)               AS n
        FROM (
            SELECT e.corpid, e.roomid, e.occurred_on,
                   GREATEST(0, (TO_DAYS(e.first_agent_reply_time) - TO_DAYS(e.first_msg_time)) * 45000
                       + LEAST(GREATEST(TIME_TO_SEC(e.first_agent_reply_time), 30600), 75600)
                       - LEAST(GREATEST(TIME_TO_SEC(e.first_msg_time),         30600), 75600)) AS secs
            FROM b_merchant_group_event e
            -- 与 metrics::group_rows 同一条 WHERE：只算商家发起且已回复的事件
            WHERE e.asker_role = 'EXTERNAL' AND e.first_agent_reply_time IS NOT NULL
        ) s
    ) w
    GROUP BY corpid, roomid, occurred_on
) q ON q.corpid = m.corpid AND q.roomid = m.roomid AND q.occurred_on = m.dt
SET m.first_reply_p50_sec = q.p50, m.first_reply_p90_sec = q.p90;
```

**没有已回复商家事件的群日不在 JOIN 里，保持原样** —— 它们两列本来就是 NULL
（两种口径下 `pct` 都在空集合上返回 `None`），不需要处理，也不该被写成 0。

按规范「数据订正前先 SELECT 确认」：跑之前把上面 `UPDATE ... SET` 换成
`SELECT m.corpid, m.roomid, m.dt, m.first_reply_p50_sec AS old_p50, q.p50 AS new_p50` 看一眼，
`new_p50 <= old_p50` 应当处处成立 —— 出现变大的行说明常量抄错了。

⚠️ **BI 看板会看到一个台阶**：迁移那一刻起，历史首响数字整体变小。跑之前先告诉用报表的人。

### 已有库增加客服日消息量表

部署包含 `b_merchant_group_agent_msg_daily` 的版本前，先在目标库建表；新建库直接执行根目录 `schema.sql`
（那份里已有这张表，DDL 以它为准，这里只是把同一段抄出来给已建库用）。
**启动自检查这六张表，缺表会在第一秒报错**，不会等到落库那一步。

```sql
CREATE TABLE b_merchant_group_agent_msg_daily (
    id                BIGINT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT '主键ID',
    corpid            VARCHAR(32)     NOT NULL COMMENT '企业ID',
    room              VARCHAR(64)     NOT NULL COMMENT '群ID。与客服日指标表同粒度，跨群总量查询时SUM',
    agent             CHAR(16)        NOT NULL COMMENT '平台客服（INTERNAL）easyUserId',
    dt                DATE            NOT NULL COMMENT '统计日',
    msg_count         INT UNSIGNED    NOT NULL COMMENT '该客服该日在该群发的消息条数。只数INTERNAL，不依赖抽取与词表',
    gmt_created_time  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP COMMENT '创建时间',
    gmt_modified_time DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP COMMENT '更新时间',
    PRIMARY KEY (id),
    UNIQUE KEY uk_agent_msg_daily (corpid, room, agent, dt) COMMENT '语义键四列：REPLACE覆盖写靠它触发冲突',
    KEY idx_agent (agent, dt) COMMENT 'BI直连：某客服某时间段（语义键前缀是corpid，按人查走不到）',
    KEY idx_day (dt) COMMENT 'BI直连：跨群跨人看某时间段'
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_general_ci COMMENT = '客服维度日消息量';
```

**不回填历史。** 这张表由跑批的事实阶段写，只覆盖此后跑到的窗口；冻结区没有行。
真要历史数据，只能用 `backfill` 重跑那段窗口 —— 那会连同事实一起重抽，
是另一件事，需要单独评估。

⚠️ 这张表**不参与打标与重打标**：`recompute` 和 `recover` 一行都不碰它。
抽取失败的群这张表照写（消息数不依赖模型），所以它的行数和
`b_merchant_group_agent_metric_daily` 对不上是正常的，不是漏写。

### 已有库增加客服账号字段

部署包含 `official_user_id` 的版本前，先在目标库执行以下一次性变更；新建库直接执行根目录 `schema.sql`。

```sql
ALTER TABLE b_merchant_group_agent_metric_daily
    ADD COLUMN official_user_id VARCHAR(64) NULL
    COMMENT '平台客服账号，对应sender.officialUserId；取本次群处理窗口最新非空值，缺失为NULL，不参与语义键'
    AFTER agent;
```

`agent` 继续保存 `easyUserId`。新增列历史值为 NULL，后续跑批写入窗口内最新非空的内部员工账号；重打标保留已有账号，不从事件推断或补填。此变更不会自动回填窗口外历史数据。

### 已有库增加末条角色字段

部署包含 `last_msg_role` 的版本前，先在目标库执行以下一次性变更；新建库直接执行根目录 `schema.sql`。

```sql
ALTER TABLE b_merchant_group_event
    ADD COLUMN last_msg_role ENUM('EXTERNAL','INTERNAL') NULL
    COMMENT '末条来源消息发送方的identityType。EXTERNAL=商家说完没人接，INTERNAL=客服收的尾。NULL=加这一列之前抽取的历史行'
    AFTER summary;
```

**历史行永远是 NULL，不回填** —— 冻结区事实列不可写（承重不变量 1），
而这一列归事实列。工作台上历史行显示为「暂无数据」，不是某一边（承重不变量 4 的形状）。
上线当天起的新事实才有值；跑批不做任何回填动作。

### 已有库增加后续等待字段

部署包含 `followup_wait_max_sec` 的版本前，先在目标库执行以下一次性变更；
新建库直接执行根目录 `schema.sql`。

```sql
ALTER TABLE b_merchant_group_event
    ADD COLUMN followup_wait_max_sec INT UNSIGNED NULL
    COMMENT '后续轮次最长等待秒数（工作时段口径08:30-21:00，周末节假日不扣）。0=确实没有后续轮次，NULL=没算过'
    AFTER last_msg_role;
```

同样**不回填**，理由同上一节。这一列曾经是全站唯一的工作时长口径；
首响时效也改成同一口径之后（见下一节），两者不再需要并排标注不同口径。

⚠️ **可改性仍然不同**：首响由 `first_msg_time` / `first_agent_reply_time` 两列在查询期
现算，换口径重算一遍就行、历史全适用；而后续等待查询期拿不到中间轮次，只能在抽取时算，
**口径在写入那一刻定死**，且 max 落在哪一轮会随口径变，事后换不回来。

⚠️ **列缺失时启动检查直接报错**（`check_schema` 读的是 `EVENT_COLS`），不会跑完一轮才在落库炸掉。
它**不进 `idx_overview`**：今天只在事件明细逐行显示，没有聚合点；将来若要按它出全局占比，
再评估加进那条覆盖索引，否则 47 万行的聚合会回表。

### 验证命令

```bash
cargo fmt --check          # 必须干净，默认配置就是本仓库风格
cargo clippy -- -D warnings
cargo test                 # 离线用例
cargo run --example dry    # 分段与 prompt 冒烟，不花 token
cargo run --example smoke  # ⚠️ 会真调端点、真花钱
```

默认测试含本地 HTTP 模型协议，不访问真实模型。真实 MySQL 验证单独启用：

```bash
# 使用隔离 MySQL，专用数据库名必须以 _test 结尾；用例自行创建、删除各自的临时库。
CHAT2EVENTS_TEST_DATABASE_URL=mysql://root@127.0.0.1:3306/chat2events_test \\
  cargo test --locked mysql_ -- --ignored
```

**本机没有 MySQL 时起一个一次性实例**（本机不装 MySQL 是常态，别为跑测试装一台）：

```bash
docker run -d --name c2e-mysql-test \\
  -e MYSQL_ROOT_PASSWORD=root -e MYSQL_DATABASE=c2e_test \\
  -p 3307:3306 mysql:8.4
docker exec c2e-mysql-test mysqladmin ping -uroot -proot   # 等它回 "mysqld is alive"

CHAT2EVENTS_TEST_DATABASE_URL='mysql://root:root@127.0.0.1:3307/c2e_test' \\
  cargo test --locked mysql_ -- --ignored

docker rm -f c2e-mysql-test                                # 用完删掉，数据不留
```

⚠️ **库名必须以 `_test` 结尾**（`testutil::mysql_pool` 对此有断言，不满足直接 panic），
端口用 3307 避开本机可能已有的 3306。**绝不要把这个变量指向 secrets 里的开发库** ——
那是一台共享远程服务器，而测试助手会在连接指向的实例上**建库删库**。

覆盖单群两日事务、失败保留旧事实、空结果清除、跨日移动、重打标、只读快照、HTTP 取数和原文缺失。
CI 使用独立 MySQL 8.4 执行这些用例。不要把该变量指向业务库；默认忽略的真实 OSS 用例不在 `mysql_` 过滤范围中。
真实模型业务质量、OSS 权限与 Linux 部署仍须独立验收。

## 只读工作台

webUI 是**两个进程**：Rust 只读 JSON 后端（`webui`）＋ 前端静态站。
生产上前端是打好包的静态文件、由 nginx 托管；开发期用 Vite dev server 顶上，`/api` 反代到后端。

### 生产

```bash
./webui /etc/chat2events <corpid> 127.0.0.1:8787
```

### 本地开发（两个终端）

```bash
# 终端 1：仓库根目录。第一个参数是**配置目录**，`.` = 用根目录那份 config.toml / secrets.toml
cargo run --locked --bin webui -- . <corpid>              # 默认监听 127.0.0.1:8787

# 终端 2：前端，/api 转发到 VITE_API_PROXY（默认 127.0.0.1:8787）
cd webui && pnpm install --frozen-lockfile && pnpm dev    # http://localhost:5273
```

`<corpid>` 是必填位置参数，**每条查询都按它过滤**。取值查库拿：
`SELECT DISTINCT corpid FROM b_merchant_group_event;` —— 也就是 raw 镜像里
`<raw_root>/<yyyyMM>/<corpId>/` 那一层的目录名。填错不会静默返回空数据集，
`/api/meta` 直接 409「该企业尚无已落库的群日或事件」。

不接 MySQL、只想看界面：后端可以不起，开 `http://localhost:5273/?source=mock` 走模拟数据源。

与跑批共用配置文件格式，但只读取必需字段；**不需要访问 raw 镜像**。生产应为工作台配置独立的 MySQL 只读账号。
它不构造 LLM 或 OSS 客户端，不写表；监听默认仅本机，nginx 示例在 `webui/deploy/nginx.conf`。
前端的打包、子路径部署与质量检查见 `webui/README.md`。服务停止使用 SIGINT，可等待在飞请求结束。

日期查询默认最近七天，可显式选全部历史；更宽窗口仍会增加响应体和浏览器内存。
2026-09-06 的合成 10 万事件测试：约 47 MB JSON，解析校验约 441 ms、装饰聚合约 121 ms，测试进程堆约 318 MB；这是开发机合成测量，不是生产容量承诺。
AntD 由打包器按路由依赖自动分块；去掉强制合包后，最大 JavaScript 单块由约 1,149 KB 降为 603 KB，构建不再触发 900 KB 警告。

⚠️ 这份文档的部署步骤**没有在真实 Linux 机器上跑过** —— 依赖关系是从
`Cargo.toml` / `Cargo.lock` / `config.rs` / `main.rs` 读出来的，不是实测的。
第一次照着做的时候把踩到的坑补回来。
