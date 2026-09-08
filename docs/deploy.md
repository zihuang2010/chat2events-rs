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
# 产物：target/release/chat2events-rs 与 target/release/webui
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
    chat2events-rs              # 二进制，来自 target/release/
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

⚠️ 这个目录同时是 webUI 下钻的可见范围 —— 超出保留期的事件取不到原文。

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

### 事实新鲜度与查询预算升级

停止所有跑批、补标与重打标，备份后执行；新库直接使用 `schema.sql`。
若尚未升级独立打标流水线，先完成下一节，再执行本节。

```sql
ALTER TABLE b_merchant_group_metric_daily
    ADD COLUMN fact_completed_time DATETIME(6) NULL
        COMMENT '事实成功保存时间；仅成功抽取写入，打标不修改；NULL表示缺少可信完成凭据'
        AFTER agent_accounts,
    ADD KEY idx_corp_day (corpid, dt);

ALTER TABLE b_merchant_group_event
    ADD KEY idx_corp_day (corpid, occurred_on);

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
只读配置只需 `[ingest].raw_root`、`[mysql]`、`[log]`、`[web]`；只读 `secrets.toml` 只需 `[mysql].url`，无需模型或 OSS 凭据。

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
cargo run --locked --example recover -- /etc/chat2events 2026-08-01 2026-09-05
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

### 已有库增加客服账号字段

部署包含 `official_user_id` 的版本前，先在目标库执行以下一次性变更；新建库直接执行根目录 `schema.sql`。

```sql
ALTER TABLE b_merchant_group_agent_metric_daily
    ADD COLUMN official_user_id VARCHAR(64) NULL
    COMMENT '平台客服账号，对应sender.officialUserId；取本次群处理窗口最新非空值，缺失为NULL，不参与语义键'
    AFTER agent;
```

`agent` 继续保存 `easyUserId`。新增列历史值为 NULL，后续跑批写入窗口内最新非空的内部员工账号；重打标保留已有账号，不从事件推断或补填。此变更不会自动回填窗口外历史数据。

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

与跑批共用配置文件格式，但只读取必需字段；原文目录必须指向同一份 raw 镜像。生产应为工作台配置独立的 MySQL 只读账号。
它不构造 LLM 或 OSS 客户端，不写表；监听默认仅本机，nginx 示例在 `webui/deploy/nginx.conf`。
前端的打包、子路径部署与质量检查见 `webui/README.md`。服务停止使用 SIGINT，可等待在飞请求结束。

日期查询默认最近七天，可显式选全部历史；更宽窗口仍会增加响应体和浏览器内存。
2026-09-06 的合成 10 万事件测试：约 47 MB JSON，解析校验约 441 ms、装饰聚合约 121 ms，测试进程堆约 318 MB；这是开发机合成测量，不是生产容量承诺。
AntD 由打包器按路由依赖自动分块；去掉强制合包后，最大 JavaScript 单块由约 1,149 KB 降为 603 KB，构建不再触发 900 KB 警告。

⚠️ 这份文档的部署步骤**没有在真实 Linux 机器上跑过** —— 依赖关系是从
`Cargo.toml` / `Cargo.lock` / `config.rs` / `main.rs` 读出来的，不是实测的。
第一次照着做的时候把踩到的坑补回来。
