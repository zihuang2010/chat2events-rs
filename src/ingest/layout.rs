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

/// 删掉过保留期的月目录，返回删掉几个。
///
/// **本轮窗口要读的月份不可能被删** —— 保留起点锚在 `w.since()` 而不是今天，
/// 所以 `lookback_days` 配多大都不会自伤。`retention = 2` ⇒ 保留窗口最早月
/// 和它前一个月。
///
/// `%Y%m` 的字典序等于时间序，所以比一次字符串就够，不用把目录名解析回日期 ——
/// 布局只被拼，不被拆。
///
/// ⚠️ **全仓唯一一处递归删生产数据的地方。** 门槛按「必须长得像月目录」设：
/// 是目录、名字恰好 6 位数字、且严格早于保留起点。raw_root 底下别的东西一律不碰。
///
/// ⚠️ **它同时决定 webUI 下钻还能看多久以前的原文** —— 超出保留期的事件，
/// [`super::read_by_ids`] 会显式报「取不到这些 msg_id」，不会静默少给。
///
/// 删不掉只是磁盘继续涨，不是数据错 —— 记 warn，不掀翻这一轮。
pub fn prune(raw_root: &Path, w: &Window, retention: u32) -> usize {
    assert!(
        retention >= 1,
        "raw_retention_months 必须 ≥ 1（0 会把本轮要读的月份删掉），改 config.toml"
    );
    let keep_from = w
        .since()
        .checked_sub_months(chrono::Months::new(retention - 1))
        .expect("窗口起点往前推几个月不会越界")
        .format(MONTH_FMT)
        .to_string();

    // raw_root 还不存在 = 一次都没拉过，不是错误。
    let Ok(entries) = fs::read_dir(raw_root) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.len() != 6
            || !name.bytes().all(|b| b.is_ascii_digit())
            || name >= keep_from
            || !e.path().is_dir()
        {
            continue;
        }
        match fs::remove_dir_all(e.path()) {
            Ok(()) => {
                n += 1;
                tracing::info!(month = %name, keep_from = %keep_from, "删除过保留期的月目录");
            }
            // 静默失败 = 磁盘会一直涨到没人发现为止。
            Err(err) => tracing::warn!(
                month = %name,
                "过保留期的月目录删不掉，磁盘会继续涨：{err}"
            ),
        }
    }
    n
}
