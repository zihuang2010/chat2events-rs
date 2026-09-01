# ADR-0005：拉取靠索引表 + HTTP Range 增量，本地是 OSS 的月文件镜像

- 状态：**已采纳**（2026-08-31）
- 相关：`src/` ① 摄取（`pull.rs` / `ingest.rs`）
- 取代：`sync.sh`（一条 `ossutil sync`），该文件已删除

## 背景：上游的真实形状，和文档里假设的完全不同

文档一直假设的路径是 `<env>/…/<yyyymmdd>/<corpid>/<roomid>.ndjson`，一个文件 = 一个群 × 一天。
`open-questions.md` 待确认②问的就是这个。**答案是第三种，三处都不一样：**

| | 假设 | 实际 |
|---|---|---|
| 分片粒度 | 一天 | **一个月**，`<yyyyMM>/<corpId>/<roomId>.ndjson` |
| 怎么发现文件 | 按日期 glob 目录 | **查 MySQL 索引表** `b_wecom_group_message_month_file` |
| 写入方式 | 每天一个新文件 | **OSS AppendObject**，每天往同一个文件尾部追加 |

索引表给的东西比路径多得多：

| 字段 | 说的事 |
|---|---|
| `ndjson_object_key` | 相对 object key |
| `ndjson_position` | 「下一批 AppendObject 预期字节位置」= **权威的文件末尾** |
| `ndjson_record_count` | 已确认追加记录数 = **免费的完整性校验** |
| `file_status` / `is_deleted` | 0 正常 / 1 冻结；筛选条件 |
| `lease_token` / `version` | 上游追加时持租约 + 乐观锁（我们只读，不用） |

## 决策

1. **拉取查索引表，不 glob 目录，不用 `ossutil sync`。**
2. **HTTP `Range: bytes=<本地字节数>-` 增量取**，走 `DOWNLOAD_BASE_URL` + `object_key`。
3. **严格只读到 `ndjson_position` 为止，一个字节都不多读。**
4. **本地是 OSS 的字节级镜像**：`<RAW_ROOT>/<yyyyMM>/<corpId>/<roomId>.ndjson`。
   于是「已拉到第几字节」= `os.path.getsize()`，**不需要任何额外的状态存储**。
5. **`getsize == ndjson_position` 是端到端校验**；不等 → 该群失败，不参与本轮跑批。
6. **本地比上游长 → 本地作废重拉**（上游删档重建才会发生，不处理就永远卡死）。
7. 失败按**群**隔离；索引表本身查不到是**整轮**失败。

## 为什么不是 `ossutil sync`

`sync` 看到文件变了就整个重下。月文件意味着**月末重下的是整月**：

| | 全量 sync | Range 增量 |
|---|---|---|
| 月初 | 0.6 MB / 群 | 0.6 MB / 群 |
| 月末 | **18 MB / 群** | 0.6 MB / 群 |
| 1000 群月末一天 | **18 GB** | **600 MB** |

而且 `ossutil` 拿不到 `ndjson_position` —— 没有它，「文件尾部那半行是不是一次在飞的追加」
就只能靠事后检测坏行。**规则 3 把那一整类失败从结构上消灭了**，这比省带宽更值钱。

## 为什么是「月文件镜像」而不是「按天分片」

设计中途选过「把每天拉到的增量各存一个小文件」，理由是「跑批只扫最近几个分片」。
**两个论据都不成立：**

**① 不安全。** 分片名只能是**拉取日期**，而里面消息的日期不保证 —— 上游追加晚了、
或者漏跑一天，消息就落在预期之外的分片里，**静默漏掉**。要做到精确就得让 `pull`
解析 `messageTime`，那会破坏「上游字段名只出现在 `ingest` 里」。

**② 不需要。** 实测 DuckDB 全投影 + `ORDER BY` 吞吐 **537 MB/s**，单文件固定开销 **35 ms**：

| | 扫描字节 | 纯扫描 | ＋每群 35 ms 固定开销 | 合计 |
|---|---|---|---|---|
| 读整月 | 18 GB | 34 s | 35 s | **~70 s** |
| 窄读 2 天 | 1.2 GB | 2 s | 35 s | **~37 s** |

**窄读一共省 33 秒**，而一次跑批光 LLM 调用就是几十分钟。

月镜像还连带消掉一串东西：状态从「各分片大小求和」变成 `getsize()`、本地路径不用发明
命名规则、`read_by_ids` 不用挑分片（单群单月 18 MB ≈ 70 ms）、Parquet 副本连同
「迟到消息弄脏了哪几天要重写」那套逻辑一起不用写。

**Parquet 的触发条件写死**：一次跑批的扫描总时长超过 5 分钟再上（今天 ~70 秒，4 倍余量）。

## 跨月

窗口 `[T-N, T-1]` 在每月头几天会跨月。全部跨月逻辑收敛成一个函数
`ingest.months(days) -> ["202608", "202609"]`，`pull` 查索引表、`list_rooms` 遍历目录、
`read_room` 选文件三处共用。

⚠️ 它依赖一条上游不变量：**`file_month` 是「消息月份」不是「接收月份」**（索引表 DDL 的注释）。
若这条不成立，8 月 31 日深夜的消息会落进 9 月文件，而窗口整个在 8 月时我们不会打开 9 月文件
—— **静默漏掉**。`_to_messages` 里有守卫（读到的文件里出现别的月份就显式失败），但它只能证伪：
窗口整个落在单月时拦不住。**已认领的代价**，实际敞口≈0（跨月窗口本来就读两个月），
`ingest.rs` 里的 `月份守卫的已知盲区` 那条测试把它写下来了。

## ⚠️ CDN 缓存：一个还没解决的外部问题

`filet.jdd51.com` 是 CDN。实测（2026-08-31）：

```
x-oss-object-type: Appendable          ✅ 确认追加写
x-oss-next-append-position: 21103      ✅ 与索引表 ndjson_position 逐位相同
accept-ranges: bytes / Range → 206     ✅ 增量可用
x-swift-cachetime: 2592000             ⚠️ 30 天缓存
```

`Cache-Control: no-cache` 请求头**不起作用**，加随机 query 参数**也不起作用** ——
**客户端绕不掉**。一个每天都在追加的文件配 30 天缓存，理论上会连续多天拿到旧副本。

**兜底**：字节数校验（规则 5）→ 拿到旧副本就是该群失败、记 `run_failure`，
数据不会写坏。**但一个群可能连续失败很多天。** 不写自动重试退避 ——
30 天的缓存不是重试几次能解决的，写了只会掩盖问题。

已列为 `open-questions.md` 第 6 条：**能否调短这条路径的 CDN TTL，或给一个绕过 CDN 的直连地址。**

## 实测（2026-08-31，dev 索引表 4 行）

```
冷启动    3 个文件落地 8620 / 4925 / 21103 字节，与 ndjson_position 逐位相同
          行数 11 / 5 / 24，与 ndjson_record_count 逐位相同
幂等      再拉一次：新增 0 · 已最新 3 · 零字节下载
增量      人为截到 8366 字节 → 只补 12737 字节 → 回到 21103 / 24 行
Range     切在真实行边界上，两段拼接与全量下载**逐字节相同**
```

## 一个环境坑，记在这里免得再踩

**Python 版踩过**：python.org 的 macOS framework Python 没有 `etc/openssl/cert.pem`
（要手工跑 `Install Certificates.command`），于是 `urllib` 报
`CERTIFICATE_VERIFY_FAILED`，而同一台机器上 `curl` 通 —— 看起来像 TLS 拦截，
其实是解释器没有根证书。那边的解法是显式用 `certifi.where()` 建 SSL context。

**Rust 版不会踩这一个，但要选对 feature。** `reqwest` 默认走
`rustls` + `webpki-roots`（证书打进二进制，不看系统证书库）—— 这坑天然不存在，
代价是换成企业内网自签 CA 时要显式加根证书。若改用 `native-tls` /
`rustls-tls-native-roots` 就会去读系统证书库，那时这个坑会以另一种形式回来。

**落地记录（2026-09-01，`pull.rs` 搬完）：我们一个 feature 都没显式选，继承的是
`rustls` + `rustls-platform-verifier`（内含 `rustls-native-certs`）—— 也就是
「读系统证书库」那一档，不是打包 `webpki-roots` 那一档。**
`Cargo.toml` 里 `reqwest = { default-features = false }`，TLS 是 `async-openai`
拉进来、经 cargo 的 feature 统一共享过来的。所以：

* 今天 `pull.rs` 和 `llm.rs` 用的是同一个 TLS 栈，两个端点实测都通，无需任何改动。
* 但这意味着**根证书跟着机器走**。哪天出现「代码没问题、这台机器下不下来」，
  先查系统证书库，而不是查代码 —— 跟 Python 版那个坑是同一类，只是换了个抽屉。
* 要变成「证书打进二进制」，得在 `Cargo.toml` 给 reqwest 显式开
  `rustls-tls-webpki-roots`。**没这么做，因为没有需求**：一个适配器 = 假想接缝，
  同一条规矩。

## Rust 版实测（2026-09-01，dev 索引表 10 行）

```
冷启动    10 个月文件落地 107295 字节 / 100 行，与 ndjson_position、
          ndjson_record_count 逐位相同
幂等      再拉一次：新增 0 · 已最新 10 · 零字节下载
增量      人为截到 18843 字节（第 20 行末）→ 只补 6213 字节 → 回到 25056 字节，
          与全量下载 **sha256 逐字节相同**
超长      人为多追加 52 字节 → WARN「本地比上游长，作废重拉」→ 重下 3200 字节，
          与原文件逐字节相同
行边界    人为截到 20000 字节（**行中间**）→ 该群失败、一个字节都没写
```

⚠️ 最后一条顺带暴露了一个**已认领的代价**：`have` 落在行中间时，`have < position`
不触发规则 3（那条只管 `have > position`），于是这个群会**每天失败、永远卡住**，
要人工删文件才恢复。不修，因为 `have` 只由 `pull` 自己写，而它只写通过校验的完整
行块 —— 行中间的 `have` 意味着磁盘损坏或有人手工改过，那在信任边界之外。
