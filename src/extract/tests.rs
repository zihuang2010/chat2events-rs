//! `extract` 的**跨文件**测试与共享 fixture —— `extract` 的子模块，六个实现文件的
//! 私有项照常可见（子模块的私有项经 `pub(super)`）。
//!
//! **单个文件的单元测试住在各自文件底部**（`redact` / `render` / `segment` / `model` /
//! `assemble` 各自的 `#[cfg(test)] mod tests`，fixture 从这里借 ——
//! `super::super::tests::{msgs, draft, …}`）。留在这里的是跨文件的性质测试
//! （划分 / 便签流动 / 占位符与上限一致性 / 端到端）和 [`BisectStub`]。
//!
//! 断言集**逐条搬自 Python 版的 `_self_check`**（`../pychat2events/src/extract.py:943`）。
//! 那 293 行是这条链上唯一一套被真实样本验证过的断言，不重新发明。
//!
//! 与 Python 的一处不同：那边的自检吃真实样本（`RAW_ROOT` 里的 3742 条），这边多数
//! 用例自己造消息 —— 造的比借的快，且不依赖另一个仓库的样本文件（那个文件**会被就地
//! 替换**，3742 → 823 已经发生过）。**逐字节对拍另有其人**：`examples/dry.rs`（实测 823 条样本双方 59664 字节相同）。

use super::{model::*, pipeline::*, prompt::*, redact::*, types::*, *};
use crate::{
    ingest::{Message, Role},
    testutil,
    window::Window,
};
use chrono::{Duration, NaiveDate};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

// ─────────────────────────────────────────────────────────────────────────────
// fixture —— 子模块的测试也用（`super::super::tests::{…}`）
// ─────────────────────────────────────────────────────────────────────────────

/// 测试用段长。生产值来自 config.toml 的 `segment_msgs`（无默认值，缺失即报错）；
/// 测试只要「够大以致不额外分段」，具体数不承重 —— 曾经 6 处裸写 400。
pub(crate) const SEG: usize = 400;

pub(crate) fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 8, 29).unwrap()
}

/// 造 n 条消息：默认间距 30s，`big_gap_at` 处改成 8 小时（给 `cut` 一个真实的隔夜断点）。
///
/// 发言人 4 个：2 个平台（偶数下标）2 个商家，够让 `labels` 有东西可分。
pub(crate) fn msgs_with(n: usize, big_gap_at: Option<usize>) -> Vec<Message> {
    let base = day().and_hms_opt(9, 0, 0).unwrap();
    let mut out = Vec::with_capacity(n);
    let mut at = base;
    for i in 0..n {
        if i > 0 {
            at += if Some(i) == big_gap_at {
                Duration::hours(8)
            } else {
                Duration::seconds(30)
            };
        }
        let internal = i % 2 == 0;
        out.push(Message {
            msg_id: format!("m{i:05}"),
            room: "R".into(),
            corp: "C".into(),
            at,
            sender_id: format!("u{}", i % 4),
            sender_role: if internal {
                Role::Internal
            } else {
                Role::External
            },
            text: format!("第 {i} 条"),
            reply_to: None,
        });
    }
    out
}

pub(crate) fn msgs(n: usize) -> Vec<Message> {
    msgs_with(n, None)
}

pub(crate) fn draft(idx: &[usize], summary: &str, still_open: bool) -> Draft {
    Draft {
        idx: idx.to_vec(),
        summary: summary.into(),
        still_open,
    }
}

/// 自检用的 `SegmentModel` 适配器 —— **第二个适配器，和 [`LiveModel`] 同一个接缝**。
///
/// 段长超过 `cap` 就说吃不下，逼出二分（`cap = 0` 则恒失败，用来验「切不动时显式抛」）。
///
/// **区间列表由真实的 [`view`] + [`merge`] 还原，不靠打桩**：每段返回一个覆盖首尾两行
/// （`msg_indexes = [1, segment_size]`）的事件，`merge` 把段内行号换算成全局下标之后，
/// drafts 里就留下了 `(lo, hi-1)` 的足迹 —— [`Self::spans`] 读回来就是实际跑过的区间。
/// 这是「不打桩 [`one_call`] 也能断言划分性质」的全部机关。
///
/// `still_open` 默认 `false`：划分性质与便签无关，而开着的 draft 每段都要被 `note`
/// 遍历一遍 —— 全开着时 `cap = 1` 会让自检退化成 O(段²)。验便签流动的那条单独传 `true`。
struct BisectStub {
    cap: usize,
    still_open: bool,
    /// 每成功处理一段，记下**进来时**便签上的 ref。
    entering: Mutex<Vec<BTreeSet<u32>>>,
}

impl BisectStub {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            still_open: false,
            entering: Mutex::new(Vec::new()),
        }
    }
    fn open(cap: usize) -> Self {
        Self {
            cap,
            still_open: true,
            entering: Mutex::new(Vec::new()),
        }
    }
    /// 实际跑过的区间，**按 ref 递增序** —— ref 是递增分配的，等价于调用顺序。
    fn spans(drafts: &BTreeMap<u32, Draft>) -> Vec<(usize, usize)> {
        drafts
            .values()
            .map(|d| (d.idx[0], d.idx[d.idx.len() - 1] + 1))
            .collect()
    }
}

impl SegmentModel for BisectStub {
    async fn call(
        &self,
        _text: &str,
        segment_size: usize,
        open_refs: &BTreeSet<u32>,
    ) -> Result<Vec<EventDraft>, SegError> {
        if segment_size > self.cap {
            return Err(SegError::TooBig("stub 吃不下".into()));
        }
        self.entering.lock().unwrap().push(open_refs.clone());
        // 走真实的 validate —— 桩也要过校验，否则测的就不是生产那条路
        validate(
            vec![EventDraft {
                r#ref: None,
                msg_indexes: vec![1, segment_size],
                summary: "自检桩".into(),
                still_open: self.still_open,
            }],
            segment_size,
            open_refs,
        )
        .map_err(|e| SegError::Failed(e.into()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 划分性质 —— 「不需要去重」的全部理由，错了就是静默的数据损坏
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn bisection_cuts_the_messages_into_a_partition_for_every_cap() {
    let ms = msgs(200);
    for cap in [1usize, 7, 50, 100, 10_000] {
        let mut drafts = BTreeMap::new();
        run(&BisectStub::new(cap), &ms, 0, ms.len(), &mut drafts, SEG)
            .await
            .unwrap();
        let seen = BisectStub::spans(&drafts);

        // 展平后必须恰好是 0..n-1：不重、不漏、顺序不乱，三条一起验。
        // 段内连续由 (lo, hi) 的形状保证；merge 换算错了这里立刻红。
        let flat: Vec<usize> = seen.iter().flat_map(|&(lo, hi)| lo..hi).collect();
        assert_eq!(
            flat,
            (0..ms.len()).collect::<Vec<_>>(),
            "上限 {cap}: 不是划分（重/漏/乱序）"
        );
        assert!(
            seen.iter().all(|&(lo, hi)| hi - lo <= cap),
            "上限 {cap}: 有段超过上限"
        );
    }
}

#[tokio::test]
async fn bisecting_down_to_one_message_and_still_failing_is_raised_not_swallowed() {
    let ms = msgs(16);
    let e = run(
        &BisectStub::new(0),
        &ms,
        0,
        ms.len(),
        &mut BTreeMap::new(),
        SEG,
    )
    .await
    .unwrap_err();
    assert!(matches!(e, SegError::TooBig(_)), "切不动了必须显式抛：{e}");
}

// ─────────────────────────────────────────────────────────────────────────────
// 便签跨段流动 —— run + merge + note 三件事合一的性质
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn notes_flow_across_segments_because_the_room_shares_one_drafts() {
    // 桩每段新开一个 ref（1,2,3…）—— 于是第 k 段进来时便签上最大的 ref 必须恰好是 k：
    // 为空说明 drafts 每段都重置了（=又回到了接缝），对不上号说明 ref 串了。
    let ms = msgs(300);
    let stub = BisectStub::open(usize::MAX);
    let events = extract(&ms, &stub, 100).await.unwrap();
    let entering = stub.entering.lock().unwrap().clone();

    assert!(
        entering.len() >= 3,
        "应该切成 >=3 段，实际 {}",
        entering.len()
    );
    assert!(entering[0].is_empty(), "第一段不该有便签");
    for (k, s) in entering.iter().enumerate().skip(1) {
        assert_eq!(
            s.iter().max().copied(),
            Some(k as u32),
            "便签没有跨段流动 —— 等于回到了接缝"
        );
    }
    assert_eq!(
        events.len(),
        entering.len(),
        "全群共用一套 drafts，ref 不该串号"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 跨文件的一致性 —— 多处必须逐字一致、其中一处是不可动的 prompt
// ─────────────────────────────────────────────────────────────────────────────

/// 三处占位符必须逐字一致：[`body`] 产出 · [`PLACEHOLDER`] 拦截 · [`SYSTEM`] 教模型认。
///
/// 改了 const 而忘了 prompt（或反过来）不会有任何编译错误，后果却是承重的：
/// 模型不知道 `<手机号>` 是脱敏记号，可能拿两条都带它的消息当同一个客户
/// （prompt 明令禁止的事），而 validator 也不再拦得住它进 `summary` ——
/// **`sha256(summary)` 是 ⑤ 的缓存键，PII 进去就焊死了**。
#[test]
fn the_prompt_and_the_masks_agree() {
    for m in [MASK_PHONE, MASK_FIELD, MASK_AT] {
        assert!(SYSTEM.contains(m), "prompt 没教模型认「{m}」");
        assert!(
            PLACEHOLDER.is_match(m),
            "validator 拦不住 summary 里的「{m}」"
        );
    }
    // 反向：body 真的产出这三个（不是只在文档里一致）。
    // ⚠️ 手机号必须放在**字段锚点之外** —— `客户电话:138…` 会被 FIELD 整段掩成
    //    `<略>`，`<手机号>` 根本轮不到出场（顺序是承重的，见 ADR-0001）。
    let mut m = msgs(1).remove(0);
    m.text = "@李培尚 打不通 13581496310 / 客户姓名：张三".into();
    let s = body(&m);
    for mask in [MASK_PHONE, MASK_FIELD, MASK_AT] {
        assert!(s.contains(mask), "body 没产出「{mask}」：{s}");
    }
}

/// prompt 教模型输出的四样，必须**恰好**是 [`EventDraft`] 声明的四个字段。
///
/// 这两处是同一件事的两份说法：schema 决定模型能输出什么，prompt 决定它以为该输出
/// 什么。给 `EventDraft` 加一个字段而忘了改 prompt，模型不会知道要填它；从 prompt
/// 里删掉一样而 schema 还留着，strict 模式会要求一个模型没被教过的键。两个方向都
/// 编译得过、都不报错，只是抽取质量安静地掉下来。
///
/// 与 [`the_prompt_and_the_masks_agree`] 同一模式：prompt 保持逐字搬运的字面量，
/// 一致性交给测试。**只查字段名，不查那段散文** —— 散文是要随样本调的，
/// 钉住它只会制造每改一次 prompt 就要更新一次的噪声。
#[test]
fn the_prompt_teaches_exactly_the_fields_the_schema_declares() {
    let schema = serde_json::to_value(schemars::schema_for!(EventDraft)).unwrap();
    let fields: BTreeSet<&str> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields,
        BTreeSet::from(["ref", "msg_indexes", "summary", "still_open"]),
        "EventDraft 的字段变了 —— 先确认 prompt 那份清单跟着改了，再改这条断言"
    );
    for f in &fields {
        assert!(
            SYSTEM.contains(&format!("- {f}：")),
            "prompt 没教模型输出「{f}」"
        );
    }
    // 反向由上面那条集合相等兜着：给 EventDraft 加字段会当场红，逼人去看 prompt。
    // **不去数 prompt 里的条目总数** —— 那会把「群里的两方」「回复箭头的三种含义」
    // 这些散文条目也算进来，改一句话就红一次，正是这条注释开头说的噪声。
}

/// `SUMMARY_MAX` 的一致性：const（validator / assemble 双保险用它）· prompt ·
/// schemars description · `schema.sql` 的列注释，四处都写着「100」，却只有 const
/// 参与编译 —— 改上限而漏改其余三处没有任何编译错误，模型仍被教着写旧上限。
/// 与 [`the_prompt_and_the_masks_agree`] 同一模式：prompt 保持字面量（逐字搬运
/// 不能动），一致性交给测试。
#[test]
fn the_prompt_the_schema_and_summary_max_agree() {
    assert!(
        SYSTEM.contains(&format!("不超过 {SUMMARY_MAX} 字")),
        "prompt 教的上限跟 SUMMARY_MAX 不一致"
    );
    let schema = serde_json::to_value(schemars::schema_for!(EventDraft)).unwrap();
    let desc = schema["properties"]["summary"]["description"]
        .as_str()
        .unwrap();
    assert!(
        desc.contains(&format!("≤{SUMMARY_MAX} 字")),
        "schemars description 跟 SUMMARY_MAX 不一致：{desc}"
    );
    let ddl = include_str!("../../schema.sql");
    assert!(
        ddl.contains(&format!("≤{SUMMARY_MAX}字")),
        "schema.sql 的 summary 列注释跟 SUMMARY_MAX 不一致"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 端到端：走真实的 ①② 读取路径，接上桩模型
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn extract_runs_end_to_end_on_a_room_read_through_ingest() {
    let root = testutil::fresh_root("extract", "e2e");
    let base = testutil::upstream_ms(day().and_hms_opt(9, 0, 0).unwrap());
    let rows: Vec<_> = (0..40)
        .map(|i| {
            serde_json::json!({
                "schemaVersion": 1, "parserVersion": 1,
                "corpId": "C", "officialRoomId": "R",
                "sourceMessageId": format!("m{i:03}"),
                "standardType": "TEXT", "messageTime": base + i * 30_000,
                "sender": {"easyUserId": format!("u{}", i % 3), "officialUserId": null,
                           "identityType": if i % 2 == 0 {"INTERNAL"} else {"EXTERNAL"}},
                "content": "原文", "analysisText": format!("5127366458053009{i:03}  第 {i} 条"),
                "semanticPayload": {"replyTo": null}
            })
        })
        .collect();
    testutil::write_month(&root, "202608", "C", "R", &rows);

    let w = Window::span(day(), day());
    let conv = crate::ingest::read_room(&root, "C", "R", &w).unwrap();
    assert_eq!(conv.msgs.len(), 40);

    let events = extract(&conv.msgs, &BisectStub::new(usize::MAX), 15)
        .await
        .unwrap();
    // 桩每段出一个事件，40 条 / 段长 15 -> 3 段
    assert_eq!(events.len(), 3);
    assert!(
        events.iter().all(|e| !e.source_msg_ids.is_empty()),
        "承重不变量 6：溯源非空"
    );
    let known: BTreeSet<&str> = conv.msgs.iter().map(|m| m.msg_id.as_str()).collect();
    assert!(
        events
            .iter()
            .flat_map(|e| &e.source_msg_ids)
            .all(|id| known.contains(id.as_str())),
        "溯源 ID 必须真实存在于该次抽取的消息里"
    );
    assert!(events.iter().all(|e| e.occurred_on == day()));
}

#[tokio::test]
async fn extract_on_an_empty_conversation_is_ok_not_an_error() {
    // 承重不变量 4：Ok([]) 与 Failed 绝不混淆
    let events = extract(&[], &BisectStub::new(0), SEG).await.unwrap();
    assert!(events.is_empty());
}
