//! `taxonomy` 归纳 —— **人工触发的进程，只产词表**。
//!
//! **不做成跑批的一环**：它不写 `b_merchant_group_event`、
//! 不参与跑批、失败无所谓、不阻塞任何人。跑批不知道它存在。
//!
//! **词表现在全靠人手写。** 词表定下后不再漂移，新 event 只做分类。
//! 没有人这一关，`event_type` 这个维度会逐日漂移 —— 「取消订单 15 起」
//! 明天可能整个消失，不是数据变了而是分类方式变了，**没有报表能建在这种维度上**。
//!
//! ```text
//! read_summary_counts ──► ★ 人手写 taxonomy_vN.toml ──► Draft::load
//!                                    ▲                       │
//!                                    │                       ▼
//!                                    └──── review ──► review_vN.md（看覆盖率，不行就回去改）
//!                                                            │
//!                                                     to_sql ──► 人工执行 SQL
//! ```
//!
//! ⚠️ **机器归纳两条路都放弃了，这个模块不产候选类。**
//! A 路径（LLM 树状 map-reduce，`induce.rs`）2026-09-02 删；
//! B 路径（本地 embedding + HDBSCAN + LLM 命名，当年的 `examples/taxonomy_embed.rs`，文件已删）
//! 2026-09-03 删。留下的是人审这条路要的四件：
//! 取 summary（写词表时看有哪些说法）、草稿的形态与校验、试打产审阅报告、转 INSERT SQL。
//!
//! **文件布局**（`mod.rs` 只装模块文档、声明和导出）：
//!
//! ```text
//! taxonomy/
//!   draft.rs   草稿文件（TOML）· 校验 · 转 INSERT SQL
//!   review.rs  试打这一件事的全部：取 summary（`store` 的只读口子）· 真打一遍标
//!              （命中数 · 代表样例 · 未分类率）· 落 review_vN.md
//! ```

mod draft;
mod review;

pub use draft::{Draft, to_sql};
pub use review::{Report, render, review, review_draft, summaries};
