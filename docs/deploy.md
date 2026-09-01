# 部署（Linux）

跑批是 **T+1 定时任务，跑完即退出，没有常驻服务**。所以这份文档不讲进程守护、
不讲健康检查、不讲滚动发布 —— 只讲三件事：**怎么编出二进制、机器上要装什么、怎么定时跑**。

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
| `ca-certificates` | `rustls-platform-verifier` → `rustls-native-certs` 要读 `/etc/ssl/certs` 才能验 CDN / 端点 / MySQL 的证书 |

---

## 编译

### ⚠️ macOS 上编不出 Linux 二进制

开发机是 macOS，`cargo build --release` 出的是 Mach-O，扔到 Linux 上不能跑。
而 bundled DuckDB 是 **C++**，交叉编译要配一套 `x86_64-unknown-linux-gnu` 的
g++ 工具链 —— 三条路里最疼的一条。

**直接在目标 Linux 机（或同发行版同架构的构建机）上编。**

```bash
# 一次性准备
apt install -y build-essential ca-certificates   # 必须有 g++，DuckDB 是 C++
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

rustc -V    # 必须 ≥ 1.85：本仓库是 edition 2024
```

```bash
# 构建
cargo build --release
# 产物：target/release/chat2events-rs
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
（`objdump -T`，超过 `GLIBC_2.17` 或残留任何 `GLIBCXX_` 就 fail），同时作为
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

### 出网白名单

| 目标 | 用途 |
|---|---|
| `https://filet.jdd51.com` | CDN，拉 OSS 月文件（`ingest.download_base_url`） |
| `https://dashscope.aliyuncs.com` | ③ 抽取的模型端点（`llm.base_url`） |
| MySQL | 双向：读索引表 `b_wecom_group_message_month_file`，写 ⑦ 的三张表 + `run_failure` |

---

## 定时跑

日志**全部走 stderr**，非 tty 时自动关掉 ANSI 颜色 —— 直接进 journald 就是干净的。
**stdout 全程不写一个字节**，结果只落 MySQL。

```ini
# /etc/systemd/system/chat2events.service
[Unit]
Description=chat2events T+1 batch
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

单个群失败**不会**让整轮非零退出（承重不变量 3：群级失败隔离，该群一行不写、
记入 `run_failure`，整轮继续）。要查有没有群失败，看 `run_failure` 表和运行结束
那行汇总日志。

### 临时排障

`RUST_LOG` 存在就盖过 `config.toml` 的 `log.level`，不用改文件：

```bash
RUST_LOG=chat2events_rs=debug ./chat2events-rs /etc/chat2events
```

⚠️ 调 `debug` 不会让 async-openai 更啰嗦（它只埋了 warn 级别的点：429 限流 header、
retry-after、5xx），只会让 reqwest/hyper 变吵。

---

## 上线前的检查

```bash
cargo fmt --check          # 必须干净，默认配置就是本仓库风格
cargo clippy -- -D warnings
cargo test                 # 离线用例
cargo run --example dry    # 分段与 prompt 冒烟，不花 token
cargo run --example smoke  # ⚠️ 会真调端点、真花钱
```

⚠️ **两处没有离线测试**：`store` 的写库 SQL（要真 MySQL）和 `LiveModel`（要真端点）。
全绿 ≠ 全验过。首次上线在生产库跑一轮真的，然后查 `run_failure` 和三张表的行数。

⚠️ 这份文档的部署步骤**没有在真实 Linux 机器上跑过** —— 依赖关系是从
`Cargo.toml` / `Cargo.lock` / `config.rs` / `main.rs` 读出来的，不是实测的。
第一次照着做的时候把踩到的坑补回来。
