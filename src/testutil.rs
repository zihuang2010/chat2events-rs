//! 测试共用 fixture，只在 `cfg(test)` 下编译。
//! 此前 `ingest` 的 `raw()` 和 `mirror` 的 `tmp()` 是同一个配方写两遍。

use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// 每个用例一个独立的 raw 区，落在系统临时目录下（上一次的残留先删掉）。
pub fn fresh_root(prefix: &str, name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("c2e-{prefix}-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    root
}

/// 把若干行按生产布局写成一个月文件。路径问 `ingest::room_path` 要 —— 布局只拼一次。
pub fn write_month(root: &Path, month: &str, corp: &str, room: &str, rows: &[Value]) {
    let p = crate::ingest::room_path(root, month, corp, room);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(
        &p,
        rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
}

/// 业务本地时间 → 上游 `messageTime` 毫秒。本地时区是 `ingest::TZ`（Asia/Shanghai =
/// UTC+8）—— 「+8」这条换算只写在这一处；此前 3 个测试文件各手写一遍 `- hours(8)`。
pub fn upstream_ms(local: chrono::NaiveDateTime) -> i64 {
    (local - chrono::Duration::hours(8))
        .and_utc()
        .timestamp_millis()
}
