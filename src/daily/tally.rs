//! 一个群跑完了的结局，和跑批那一行日志要的几个数。
//!
//! [`Tally::record`] 是**承重不变量 3 的处置点** —— 它分「整轮死」和「群级跳过」
//! 两条通道，两个排空点（`run_rooms` 循环里的背压、循环后的收尾）复用同一份逻辑，
//! 抄两遍迟早抄岔。

use crate::{Result, ingest::IngestError};

/// 一个群跑完了的三种结局。抽取失败**已经落过库**（`group` 行 + `run_failure`），
/// 这里只是把数字带回去汇总。
#[derive(Debug)]
pub(super) enum Outcome {
    /// 窗口内一条消息都没有 —— 不写任何行。
    Empty,
    Ok {
        msgs: usize,
        events: usize,
    },
    Failed {
        msgs: usize,
    },
}

/// 一个群跑完了：它是谁、结局如何（或读取阶段就失败了）。
pub(super) type RoomResult = (String, String, std::result::Result<Outcome, IngestError>);

/// 跑批那一行日志要的几个数。收成一个类型是为了让 [`Self::record`] 在两个排空点
/// （循环里的背压、循环后的收尾）复用同一份处置逻辑 —— 那段逻辑分了「整轮死」和
/// 「群级跳过」两条通道，抄两遍迟早抄岔。
#[derive(Default, Debug)]
pub(super) struct Tally {
    pub(super) msgs: usize,
    pub(super) events: usize,
    pub(super) ok: usize,
    /// 窗口内没有消息 —— **既不是成功也不是失败**，一行都没写。
    pub(super) empty: usize,
    pub(super) failed: usize,
    /// 拉取阶段就失败的群，只记了 `run_failure`。
    pub(super) unsynced: usize,
    /// 整轮预算用完、根本没开始的群。**跟 `failed` 分开计** —— 那是"跑了但坏了"，
    /// 这是"没轮到"，两者下一轮的处置一样，但看日志时的诊断完全不同。
    pub(super) over_budget: usize,
}

impl Tally {
    pub(super) fn record(&mut self, (corp, room, r): RoomResult) -> Result<()> {
        match r {
            Ok(Outcome::Empty) => self.empty += 1,
            Ok(Outcome::Ok { msgs, events }) => {
                self.ok += 1;
                self.msgs += msgs;
                self.events += events;
            }
            Ok(Outcome::Failed { msgs }) => {
                self.failed += 1;
                self.msgs += msgs;
            }
            // 上游解析器变了 —— 不是某个群的事，整轮退出，不做兼容层。
            // 提前返回会把 `set` 丢掉：已经在跑的任务打断不了，但进程本来就要退了。
            Err(e @ IngestError::Upstream(_)) => return Err(e.into()),
            // 其余都是该群的事：整体跳过、一行不写、**整轮继续**（承重不变量 3）。
            Err(e) => {
                self.failed += 1;
                tracing::error!(corp = %corp, room = %room, "读取失败，该群本轮跳过：{e}");
            }
        }
        Ok(())
    }
}
