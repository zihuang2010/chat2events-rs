//! 路径布局 —— **只有这个文件知道**。`mirror` 写文件时也问这里要路径，两边必须同一个函数。
//!
//! 本地布局是 OSS 的字节级镜像（ADR-0005）：
//! `<raw_root>/<yyyyMM>/<corpId>/<officialRoomId>.ndjson`。

use crate::window::Window;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

/// 路径布局的两个字面量。布局是**拼**出来的，没有第二处再去**拆**它。
///
/// `MONTH_FMT` 另外借给 `read.rs` 的月份守卫用 —— 那里要把消息时间格式化成月份，
/// 和这里拼路径用的必须是同一个格式串，不能各写一份。
const EXT: &str = "ndjson";
pub(super) const MONTH_FMT: &str = "%Y%m";

/// 窗口覆盖的月份。每月头几天窗口会跨月，那时这里返回两个。
///
/// 上游按**消息月份**分文件（索引表 `file_month` 的注释），所以窗口里每一天
/// 的消息都落在 `months(w)` 这几个文件里，不需要额外余量。
/// 这条前提由 `scan` 的守卫④兜着 —— 不成立就是显式失败。
pub fn months(w: &Window) -> Vec<String> {
    // 窗口天数升序（Window 构造保证）⇒ 格式化出的月份天然有序，只需去重
    let mut m: Vec<String> = w
        .days()
        .iter()
        .map(|d| d.format(MONTH_FMT).to_string())
        .collect();
    m.dedup();
    m
}

pub fn room_path(raw_root: &Path, month: &str, corp: &str, room: &str) -> PathBuf {
    raw_root
        .join(month)
        .join(corp)
        .join(format!("{room}.{EXT}"))
}

/// 窗口覆盖的月份里有文件的 (corp, room)。分片键含 corpid，只返回 room 不够。
///
/// ⚠️ 「有文件」不等于「窗口内有消息」—— 判断后者要读文件，那是 [`super::read_room`]
/// 的活。窗口内一条消息都没有的群，[`super::read_room`] 返回空 `msgs`，由调用方跳过。
pub fn list_rooms(raw_root: &Path, w: &Window) -> Vec<(String, String)> {
    // BTreeSet 顺便排序：调用方按固定顺序跑批，日志和失败列表才可比。
    let mut found = BTreeSet::new();
    for m in months(w) {
        // 目录不存在 = 那个月没拉过，不是错误。
        let Ok(corps) = fs::read_dir(raw_root.join(&m)) else {
            continue;
        };
        for corp in corps.flatten() {
            let Ok(rooms) = fs::read_dir(corp.path()) else {
                continue;
            };
            for room in rooms.flatten() {
                let p = room.path();
                if p.extension().is_some_and(|e| e == EXT)
                    && let Some(stem) = p.file_stem()
                {
                    found.insert((
                        corp.file_name().to_string_lossy().into_owned(),
                        stem.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
    }
    found.into_iter().collect()
}

/// 存在的那几个月文件，**连同它属于哪个月**。跨月时某个月可能没有（新建群 / 已解散），
/// 而 DuckDB 对不存在的路径直接报错，所以必须先过一遍 `is_file`。
///
/// 月份跟着路径一起返回，是为了让 `read::scan` 的月份守卫**不必再从路径里反解一次** ——
/// 这里刚用它拼出的东西，没有道理让下游拆回来。
pub(super) fn files(raw_root: &Path, corp: &str, room: &str, w: &Window) -> Vec<(String, PathBuf)> {
    months(w)
        .into_iter()
        .map(|m| {
            let p = room_path(raw_root, &m, corp, room);
            (m, p)
        })
        .filter(|(_, p)| p.is_file())
        .collect()
}
