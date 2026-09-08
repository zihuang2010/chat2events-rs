//! 一个群抽取与保存的结果；打标队列单独汇总成功和失败。
//!
//! [`Tally::record`] 是**承重不变量 3 的处置点** —— 它分「整轮死」和「群级跳过」
//! 两条通道，两个排空点（`run_rooms` 循环里的背压、循环后的收尾）复用同一份逻辑，
//! 抄两遍迟早抄岔。

use crate::{Result, ingest::IngestError};

/// 一个群抽取阶段的结局；`Ok` 已保存事实，随后交接打标。
/// `Empty` 不写库，`Skipped` 的失败记录由调用方补齐。
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
    /// 整轮预算用完，这个群**根本没开始跑**。
    ///
    /// **不是 `f` 能返回的东西** —— `run_rooms` 在决定不调用 `f` 时自己造一个。
    /// 仍然放进 `Outcome`，是为了让四种结局都经过 [`Tally::record`] 那一个 match：
    /// 承重不变量 3 的处置点因此还是只有一处。
    Skipped,
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
    /// 整轮预算用完、根本没开始的群。**跟 `failed` 分开记** —— 那是「跑了但坏了」，
    /// 这是「没轮到」，两者下一轮的处置一样，但看日志时的诊断完全不同。
    ///
    /// ⚠️ **存名单不存计数。** 这批群还欠一行 `run_failure`，而 `run_rooms` 刻意不
    /// 认识 `store`（见它的文档注释），记账只能由调用方做 —— 名单得先出得来。
    /// 只留一个计数器时，库里对这批群是**整行缺失**：几个月后报表上的洞查不出
    /// 任何原因，因为那个数只活在这一轮的内存里。
    pub(super) skipped: Vec<(String, String)>,
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
            // 没轮到 —— 既不是成功也不是失败，且**还没落库**。名单带回去，
            // 由 `run_span` 走 `record_failure` 补上那一行。
            Ok(Outcome::Skipped) => self.skipped.push((corp, room)),
            // 上游解析器变了 —— 不是某个群的事，整轮退出，不做兼容层。
            // 提前返回会把 `set` 丢掉：已经在跑的任务打断不了，但进程本来就要退了。
            Err(e @ IngestError::Upstream(_)) => return Err(e.into()),
            // 其余都是该群的事：整体跳过、一行不写、**整轮继续**（承重不变量 3）。
            Err(e) => {
                self.failed += 1;
                tracing::error!(corp = %corp, room = %room, "该群本轮失败：{e}");
            }
        }
        Ok(())
    }
}
