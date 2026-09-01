//! ③ 抽取 extract ＋ ④ 装配 assemble —— 把会话变成结构化事件。
//!
//! ```text
//! Conversation ──切成 ceil(n / segment_msgs) 段──> 段1 →📝→ 段2 →📝→ 段K
//!                                                （全群共用一套 drafts，零接缝）
//!                                                          │
//!                                            EventDraft（模型只说四样）
//!                                                          ▼
//!                                          ④ assemble ── 11 个字段从真实消息算
//!                                                          ▼
//!                                                        Event
//! ```
//!
//! **端口是 [`SegmentModel`]**（③ 的真接缝）：换模型 / 换端点只改 [`LiveModel`]，
//! 二分逻辑一行不动。**④ `assemble::assemble` 明确不给端口** —— 它是承重不变量 6（溯源）
//! 的守卫，给它接缝等于给溯源留绕过口。
//!
//! 对外只有三样：[`Event`] · [`SegmentModel`] · [`extract`]。以下全是内部行为，
//! 不上浮到接口上：自适应二分 · 段间便签 · 序号↔`msg_id` 映射 · 正文脱敏 ·
//! schema 校验 · 溯源校验 · 重试。
//!
//! **模型根本不接触 `msg_id`**（承重不变量 6）：prompt 里给的是段内 1-based 序号，
//! 代码映射回 `msg_id`（唯一发生地是 `assemble::merge`）。序号越界直接是校验失败 ——
//! 这从根本上消灭了「模型编造 msgid」这个失败模式。
//!
//! 论证与实测数字在 **ADR-0001**（正文脱敏）· **ADR-0002**（`replyTo` 对齐）·
//! **ADR-0004**（串行链传便签、自适应二分）。搬运自 `../pychat2events/src/extract.py`。
//!
//!
//! **文件布局**（`mod.rs` 只装模块文档、声明和导出，生产代码一律在兄弟文件里；
//! 拆成目录只为导航 —— 接口一字未动，对外仍然只有上面那三样）：
//!
//! ```text
//! extract/
//!   types.rs     EventDraft · Draft · Event · SUMMARY_MAX —— 领域类型
//!   pipeline.rs  调用链：分段 → 段调用 → 自适应二分
//!   model.rs     端口 SegmentModel · 校验 · LiveModel（端点知识只在这里）
//!   redact.rs    正文脱敏与订单号正则（ADR-0001）
//!   prompt.rs    SYSTEM —— 逐字搬运，改一个字所有实测结论作废
//!   render.rs    便签 · 匿名标签 · 行号箭头，view 是唯一出口
//!   segment.rs   分段与切点选择（ADR-0004）
//!   assemble.rs  ④ merge / align / assemble / orphans
//! ```

mod assemble;
mod model;
mod pipeline;
mod prompt;
mod redact;
mod render;
mod segment;
mod types;

// 端口和它的生产适配器住在 `model`、调用链住在 `pipeline`、领域类型住在 `types`；
// 这里只把名字接出去 —— 调用方（`daily` / `store` / examples）仍然写
// `extract::LiveModel` / `extract::Event` / `extract::extract`，接口一字未动。
pub use model::{LiveModel, SegError, SegmentModel};
pub use pipeline::{extract, preview};
pub use types::{Event, EventDraft};

#[cfg(test)]
mod tests;
