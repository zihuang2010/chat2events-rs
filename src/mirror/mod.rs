//! ① 摄取 · 镜像 —— 把本地 raw 区同步成 OSS 月文件的**字节级镜像**（**ADR-0005**）。
//!
//! **模块名就是那条承重结论**：本地是镜像，所以「已经拉到第几字节」= 文件大小，
//! 不需要任何额外的状态存储。[`sync()`] 每轮把镜像推到与上游一致，推不动的群报出来。
//!
//! ```text
//! MySQL b_wecom_group_message_month_file          ← 谁、在哪、到第几字节
//!           │ ndjson_object_key
//!           ▼
//! GET <download_base_url>/<object_key>            Range: bytes=<本地已有>-
//!           │ 只取到 ndjson_position 为止
//!           ▼
//! <raw_root>/<yyyyMM>/<corpId>/<roomId>.ndjson    追加，OSS 的字节级镜像
//! ```
//!
//! **为什么不是 `ossutil sync`**：OSS 上是按月的 AppendObject，每天往里追加。
//! sync 看到文件变了就整个重下 —— 月末一个群 18 MB、1000 个群 18 GB，而当天真正
//! 新增的只有 600 MB。索引表把「新的字节从第几位开始」直接告诉了我们。
//!
//! **三条承重规则**：
//!
//! 1. **严格只读到 `ndjson_position` 为止，一个字节都不多读。** 超过它的字节可能是
//!    一次在飞的 AppendObject，末尾会是半行 JSON。这条从结构上消灭了「截断坏行」
//!    那一整类失败，比事后检测干净得多。
//! 2. **本地字节数必须等于 `ndjson_position`。** 端到端的完整性证明，同时是 CDN
//!    陈旧缓存的唯一防线 —— 那条路径配了 30 天缓存，实测 `Cache-Control: no-cache`
//!    和随机 query 参数都绕不掉（ADR-0005 结尾）。
//! 3. **本地比上游还长 → 本地作废重拉。** 追加写不该出现这种事（上游删档重建才会），
//!    但不处理就会每天校验失败、这个群永远跑不了。
//!
//! **瞬时失败共尝试 3 次**（重试 2 次，退避 1s → 2s）。1000 个群一轮就是 1000 次 GET，CDN 抖动
//! 0.5% 就是每天 5 个群从指标里静默消失 —— 不变量 3 让它一行不写，不变量 5 让那些
//! 客服的 `metric_agent_daily` 整行缺失，主管直接 `SUM` 会得到一个偏小但看起来正常
//! 的数字。只重端侧的临时状况（连接类 / 超时 / 5xx / 429 / 408，见
//! `download::http_status_error`）；**上面那三道校验一次都不重**，那是旧副本，再要还是它。
//!
//! 失败按**群**隔离（承重不变量 3），与抽取失败同一条路径；**索引表本身查不到是
//! 整轮失败** —— 那不是某个群的事。两种处置在类型上分开（[`error::MirrorError`]），
//! 跟 `ingest` 的 [`crate::ingest::IngestError`] 一个规矩。
//!
//! **路径布局不在这里** —— 写文件问 [`crate::ingest::room_path`] 要路径，跟读的是同一个
//! 函数。**索引表知识（上游字段名 · `MonthFile` · `index_sql!`）整体住在
//! `mirror/index.rs`** —— 它读 MySQL（sqlx），`download.rs` 下载落盘（reqwest + `std::fs`），
//! 两边只通过 `MonthFile` 交换数据。

//!
//! **文件布局**（`mod.rs` 只装模块文档、声明和导出，生产代码一律在兄弟文件里）：
//!
//! ```text
//! mirror/
//!   error.rs     MirrorError —— 「整轮死」与「群级跳过」两条通道的类型分界
//!   index.rs     索引表知识：上游字段名 · MonthFile · index_sql!（读 MySQL）
//!   download.rs  一个月文件怎么下：Range 请求 · 三道校验 · 瞬时失败重试
//!   sync.rs      一轮怎么调度：JoinSet 背压 · 整轮 deadline · 汇总
//! ```

mod download;
mod error;
mod index;
mod sync;

pub use sync::sync;
