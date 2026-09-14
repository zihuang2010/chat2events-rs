//! `mirror` 的跨文件测试素材。
//!
//! 今天只有一件：[`MonthFile`] 的 fixture。它此前在 `download.rs` 里被逐字构造 6 次、
//! 在 `sync.rs` 里又写了一份 helper —— 而 `MonthFile` 有 6 个字段，加一个就要动 7 处。
//! 按 `lib.rs` 的规则「跨文件的测试和**共享 fixture** 进 `<模块>/tests.rs`」，
//! `mirror` 是七个目录模块里唯一没跟上的那个。
//!
//! **变体一律走 `..file(...)` 结构更新**，不再抄整个字面量 ——
//! 各处真正不同的只有 `object_key` / `position` / `record_count` 三个字段。

use super::index::MonthFile;

/// 一个群一个月的索引行。默认是「对象已存在、8 字节 1 条记录」这个最常用的形状。
pub(super) fn file(room: &str, month: &str) -> MonthFile {
    MonthFile {
        corp: "C".into(),
        room: room.into(),
        month: month.into(),
        object_key: format!("{month}/C/{room}.ndjson"),
        position: 8,
        record_count: 1,
    }
}
