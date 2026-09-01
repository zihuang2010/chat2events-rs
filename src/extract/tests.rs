//! `extract` 的测试 —— `extract` 的子模块，六个实现文件的私有项照常可见
//! （`redact` / `render` 的私有项经 `pub(super)`，其余走 `mod.rs` 的 `use`）。
//!
//! 断言集**逐条搬自 Python 版的 `_self_check`**（`../pychat2events/src/extract.py:943`）。
//! 那 293 行是这条链上唯一一套被真实样本验证过的断言，不重新发明。
//!
//! 与 Python 的一处不同：那边的自检吃真实样本（`RAW_ROOT` 里的 3742 条），这边多数
//! 用例自己造消息 —— 造的比借的快，且不依赖另一个仓库的样本文件（那个文件**会被就地
//! 替换**，3742 → 823 已经发生过）。**逐字节对拍另有其人**：`examples/dry.rs`（实测 823 条样本双方 59664 字节相同）。

// `merge` / `align` / `assemble` / `orphans` / `cut` / `segments` 走 `super::*`
// （mod.rs 已经把它们 use 进来了）；`redact` / `render` 的其余私有项要显式 glob。
use super::{redact::*, render::*, *};
use crate::{ingest::Role, testutil, window::Window};
use chrono::{Duration, NaiveDate};
use std::sync::Mutex;

// ─────────────────────────────────────────────────────────────────────────────
// fixture
// ─────────────────────────────────────────────────────────────────────────────

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 8, 29).unwrap()
}

/// 造 n 条消息：默认间距 30s，`big_gap_at` 处改成 8 小时（给 `cut` 一个真实的隔夜断点）。
///
/// 发言人 4 个：2 个平台（偶数下标）2 个商家，够让 `labels` 有东西可分。
fn msgs_with(n: usize, big_gap_at: Option<usize>) -> Vec<Message> {
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
            sender_role: if internal { Role::Internal } else { Role::External },
            text: format!("第 {i} 条"),
            reply_to: None,
        });
    }
    out
}

fn msgs(n: usize) -> Vec<Message> {
    msgs_with(n, None)
}

/// 只为跑 `body` 的用例：一条消息，正文随便换。
fn one(text: &str) -> Message {
    let mut m = msgs(1).remove(0);
    m.text = text.into();
    m
}

fn b(text: &str) -> String {
    body(&one(text))
}

fn draft(idx: &[usize], summary: &str, still_open: bool) -> Draft {
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
/// `still_open` 默认 `false`：划分性质与便签无关，而开着的 draft 每段都要被 [`note`]
/// 遍历一遍 —— 全开着时 `cap = 1` 会让自检退化成 O(段²)。验便签流动的那条单独传 `true`。
struct BisectStub {
    cap: usize,
    still_open: bool,
    /// 每成功处理一段，记下**进来时**便签上的 ref。
    entering: Mutex<Vec<BTreeSet<u32>>>,
}

impl BisectStub {
    fn new(cap: usize) -> Self {
        Self { cap, still_open: false, entering: Mutex::new(Vec::new()) }
    }
    fn open(cap: usize) -> Self {
        Self { cap, still_open: true, entering: Mutex::new(Vec::new()) }
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
        run(&BisectStub::new(cap), &ms, 0, ms.len(), &mut drafts, 400)
            .await
            .unwrap();
        let seen = BisectStub::spans(&drafts);

        // 展平后必须恰好是 0..n-1：不重、不漏、顺序不乱，三条一起验。
        // 段内连续由 (lo, hi) 的形状保证；merge 换算错了这里立刻红。
        let flat: Vec<usize> = seen.iter().flat_map(|&(lo, hi)| lo..hi).collect();
        assert_eq!(flat, (0..ms.len()).collect::<Vec<_>>(), "上限 {cap}: 不是划分（重/漏/乱序）");
        assert!(seen.iter().all(|&(lo, hi)| hi - lo <= cap), "上限 {cap}: 有段超过上限");
    }
}

#[tokio::test]
async fn bisecting_down_to_one_message_and_still_failing_is_raised_not_swallowed() {
    let ms = msgs(16);
    let e = run(&BisectStub::new(0), &ms, 0, ms.len(), &mut BTreeMap::new(), 400)
        .await
        .unwrap_err();
    assert!(matches!(e, SegError::TooBig(_)), "切不动了必须显式抛：{e}");
}

#[test]
fn segments_is_a_partition_and_does_not_split_what_fits() {
    for n in [1usize, 2, 167, 823] {
        for cap in [1usize, 7, 500, 1000, 1_000_000] {
            let sub = msgs(n);
            let bs = segments(&sub, cap);
            assert_eq!((bs[0].0, bs[bs.len() - 1].1), (0, n), "({n},{cap}) 没覆盖到头尾");
            assert!(bs.windows(2).all(|w| w[0].1 == w[1].0), "({n},{cap}) 段之间有缝/重叠");
            assert!(bs.iter().all(|&(lo, hi)| hi > lo), "({n},{cap}) 有空段");
            assert_eq!(bs.len(), n.div_ceil(cap).max(1), "({n},{cap}) 段数不对");
        }
    }
    assert_eq!(segments(&msgs(167), 500), [(0, 167)], "装得下就必须只有一段");
}

// ─────────────────────────────────────────────────────────────────────────────
// 切点
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn cut_stays_strictly_inside_and_picks_the_largest_gap() {
    let ms = msgs_with(200, Some(96));
    for (lo, hi) in [(0usize, 200usize), (0, 2), (10, 13), (60, 130)] {
        let target = (lo + hi) / 2;
        let c = cut(&ms, lo, hi, target, hi - lo);
        assert!(lo < c && c < hi, "切点 {c} 跑出 ({lo},{hi})");
        let r = ((hi - lo) / 20).max(1);
        let win = (lo + 1).max(target.saturating_sub(r))..hi.min(target + r + 1);
        let best = win.map(|i| ms[i].at - ms[i - 1].at).max().unwrap();
        assert_eq!(ms[c].at - ms[c - 1].at, best, "({lo},{hi}) 没挑到间隔最大的");
    }
}

#[test]
fn cut_moves_off_the_middle_of_a_burst_onto_the_overnight_break() {
    // 中点 100 处是 30s 的连发；隔夜断点在 96，落在 ±5%（±10 条）窗口内
    let ms = msgs_with(200, Some(96));
    let c = cut(&ms, 0, 200, 100, 200);
    assert_eq!(c, 96, "切点没挪到隔夜断点上");
    assert!(ms[c].at - ms[c - 1].at > ms[100].at - ms[99].at, "挪过去反而更小了");
}

#[test]
fn cut_breaks_ties_on_the_lowest_index() {
    // 全是 30s，处处平手 —— Python 的 max(win, key=...) 取第一个，
    // Rust 的 max_by_key 取最后一个，反了分段边界就跟 Python 版不一致
    let ms = msgs(200);
    let target = 100usize;
    let r = 200 / 20;
    assert_eq!(cut(&ms, 0, 200, target, 200), target - r, "平局必须取窗口里最小的下标");
}

// ─────────────────────────────────────────────────────────────────────────────
// 标签 / 渲染
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn labels_are_computed_once_per_room_and_never_collide() {
    let ms = msgs(200);
    let all = labels(&ms);
    let distinct: BTreeSet<&String> = all.values().collect();
    assert_eq!(distinct.len(), all.len(), "标签撞号 —— 两个人共用一个标签");
    assert!(all.values().all(|v| v.starts_with("平台") || v.starts_with("商家")));

    // 真正要守的：render 用传进来的那份，不自己现算
    for (lo, hi) in segments(&ms, 70) {
        let out = render(&ms[lo..hi], &all, &[], &BTreeMap::new());
        assert!(
            ms[lo..hi].iter().all(|m| out.contains(&all[&m.sender_id])),
            "render 没用传入的标签"
        );
    }
}

#[test]
fn labels_roll_over_past_twenty_six_speakers() {
    let mut ms = msgs(1);
    for i in 0..27 {
        let mut m = ms[0].clone();
        m.msg_id = format!("x{i}");
        m.sender_id = format!("s{i}");
        m.sender_role = Role::Internal;
        ms.push(m);
    }
    let l = labels(&ms);
    assert_eq!(l["s0"], "平台B", "u0 先占了 平台A");
    assert_eq!(l["s25"], "平台A1", "26 个之后要进位");
}

#[test]
fn reply_arrows_render_in_segment_on_note_and_as_a_quote() {
    let ms = msgs(700);
    let l = labels(&ms);
    let tag = [(7u32, "远处那件事".to_string(), "上次说到".to_string())];
    let far_id = ms[10].msg_id.clone();
    let e7: BTreeMap<String, String> = [(far_id.clone(), "E7".to_string())].into();
    let quoted: BTreeMap<String, String> = [(far_id.clone(), "「原话」".to_string())].into();

    let seg: Vec<Message> = ms[600..610].to_vec();
    assert!(!render(&seg, &l, &tag, &e7).contains("↩回复"), "没人引用时不该冒出箭头");

    let mut far = seg.clone();
    far[3].reply_to = Some(far_id.clone()); // 指到段外
    assert!(render(&far, &l, &tag, &e7).contains("↩回复 E7"), "段外 replyTo 的箭头被丢了");
    assert!(render(&far, &l, &tag, &quoted).contains("↩回复 「原话」"), "便签上没有的该给原话");
    assert!(
        !render(&far, &l, &tag, &BTreeMap::new()).contains("↩回复"),
        "outside 没给映射时不能凭空造箭头"
    );

    let mut near = seg.clone();
    near[5].reply_to = Some(seg[1].msg_id.clone());
    assert!(render(&near, &l, &[], &BTreeMap::new()).contains("↩回复 #2"), "段内 replyTo 没渲染");
}

#[test]
fn view_keeps_open_refs_identical_to_the_note() {
    let mut ms = msgs(700);
    let far_id = ms[10].msg_id.clone();
    ms[605].reply_to = Some(far_id); // 指到段外、且挂在便签上

    let probe: BTreeMap<u32, Draft> = [(7u32, draft(&[10], "远处那件事", true))].into();
    let (text, open_refs) = view(&ms, 600, 610, &probe, 400);
    assert_eq!(open_refs, [7u32].into_iter().collect::<BTreeSet<_>>(), "open_refs 与便签不一致");
    assert!(text.contains("E7:") && text.contains("↩回复 E7"), "便签上的段外引用没接上");

    let (text2, open2) = view(&ms, 600, 610, &BTreeMap::new(), 400);
    assert!(open2.is_empty(), "没有便签时 open_refs 必须为空");
    assert!(text2.contains("↩回复 「"), "便签外的段外引用该退化成原话，不能静默丢");
    assert!(!text2.contains("【进行中的事件】"), "空便签不该渲染出标题");
}

// ─────────────────────────────────────────────────────────────────────────────
// 合并 / 对齐
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn merge_maps_segment_line_numbers_to_global_indexes() {
    let mk = |r: Option<u32>, ix: Vec<usize>, s: &str, open: bool| EventDraft {
        r#ref: r,
        msg_indexes: ix,
        summary: s.into(),
        still_open: open,
    };
    let mut md = BTreeMap::new();
    merge(&mut md, vec![mk(None, vec![1, 3], "要求取消", true)], 100);
    assert_eq!(md[&1].idx, [100, 102], "行号没换算成全局下标");

    merge(&mut md, vec![mk(Some(1), vec![2], "已取消完毕", false)], 200);
    assert_eq!(md[&1].summary, "要求取消", "summary 该保留首个（诉求）");
    assert!(!md[&1].still_open, "still_open 是状态标志，必须取最新");
    assert_eq!(md[&1].idx, [100, 102, 201], "idx 没取并集");
}

#[test]
fn align_merges_explicit_replies_and_is_identity_without_them() {
    let ms = msgs(700);
    let probe: BTreeMap<u32, Draft> = [
        (3u32, draft(&[101, 102], "晚", true)),
        (1u32, draft(&[100], "早", true)),
        (9u32, draft(&[500], "不相干", true)),
    ]
    .into();
    assert_eq!(
        align(probe.clone(), &ms).keys().collect::<Vec<_>>(),
        probe.keys().collect::<Vec<_>>(),
        "没有 replyTo 时必须是恒等变换"
    );

    let mut linked = ms.clone();
    linked[101].reply_to = Some(ms[100].msg_id.clone());
    let got = align(probe.clone(), &linked);
    assert_eq!(got.keys().copied().collect::<Vec<_>>(), [1, 9], "该并的没并 / 保留的 ref 不对");
    assert_eq!(got[&1].idx, [100, 101, 102], "idx 没取并集");
    assert_eq!(got[&1].summary, "早", "该保留 idx[0] 最小那个的 summary");
    assert_eq!(got[&9].idx, [500], "不相干的 draft 被动了");

    // 传递性：#500 再回复 #101，三个 draft 应并成一个
    linked[500].reply_to = Some(ms[101].msg_id.clone());
    let chain = align(probe, &linked);
    assert_eq!(chain.keys().copied().collect::<Vec<_>>(), [1], "传递闭包没闭合");
    assert_eq!(chain[&1].idx, [100, 101, 102, 500]);
}

// ─────────────────────────────────────────────────────────────────────────────
// 便签
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn note_evicts_the_stale_rescues_explicit_replies_and_skips_the_closed() {
    let ms = msgs(700);
    let probe: BTreeMap<u32, Draft> = [
        (1u32, draft(&[500], "近", true)),
        (2u32, draft(&[10], "远", true)),
        (3u32, draft(&[505], "闭", false)),
    ]
    .into();
    let kept = |m: &[Message], keep: usize| -> Vec<u32> {
        note(&probe, m, 600, 700, keep).into_iter().map(|(r, _, _)| r).collect()
    };
    assert_eq!(kept(&ms, 1000), [1, 2], "窗口够大时不该撤，闭合的本来就不该进");
    assert_eq!(kept(&ms, 200), [1], "窗口外的没撤下去");

    let mut pulled = ms.clone();
    pulled[650].reply_to = Some(ms[10].msg_id.clone()); // 本段显式引用那件远事
    assert_eq!(kept(&pulled, 200), [1, 2], "被 replyTo 指到的必须捞回来，多远都捞");
}

#[test]
fn note_carries_the_order_number_as_the_only_reliable_join_key() {
    let mut ms = msgs(700);
    ms[10].text = "5127366458053009229  加14个筒灯".into();
    let probe: BTreeMap<u32, Draft> = [(1u32, draft(&[10], "商家要求加单", true))].into();
    let rows = note(&probe, &ms, 600, 700, 10_000);
    assert_eq!(rows[0].1, "5127366458053009229 · 商家要求加单", "便签没带上订单号");
}

#[tokio::test]
async fn notes_flow_across_segments_because_the_room_shares_one_drafts() {
    // 桩每段新开一个 ref（1,2,3…）—— 于是第 k 段进来时便签上最大的 ref 必须恰好是 k：
    // 为空说明 drafts 每段都重置了（=又回到了接缝），对不上号说明 ref 串了。
    let ms = msgs(300);
    let stub = BisectStub::open(usize::MAX);
    let events = extract(&ms, &stub, 100).await.unwrap();
    let entering = stub.entering.lock().unwrap().clone();

    assert!(entering.len() >= 3, "应该切成 >=3 段，实际 {}", entering.len());
    assert!(entering[0].is_empty(), "第一段不该有便签");
    for (k, s) in entering.iter().enumerate().skip(1) {
        assert_eq!(s.iter().max().copied(), Some(k as u32), "便签没有跨段流动 —— 等于回到了接缝");
    }
    assert_eq!(events.len(), entering.len(), "全群共用一套 drafts，ref 不该串号");
}

// ─────────────────────────────────────────────────────────────────────────────
// 订单号
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn order_numbers_are_found_mid_body_and_phones_are_not_mistaken_for_them() {
    let hit = |s: &str| ORDER_NO.find(s).map(|m| m.as_str().to_string());
    assert_eq!(hit("5127366458053009229  加14个筒灯").as_deref(), Some("5127366458053009229"));
    assert_eq!(hit("JDLY202606271814212465\n安排师傅").as_deref(), Some("JDLY202606271814212465"));
    assert_eq!(hit("3316977912130066680====这个餐厅反馈频闪").as_deref(), Some("3316977912130066680"));
    // 平台的工单推送把单号写在正文中间 —— 锚了行首这 36 条一条都进不了便签
    assert_eq!(
        hit("工单原因：电话核实\n订单号:JDLY202608031734008496").as_deref(),
        Some("JDLY202608031734008496")
    );
    assert_eq!(hit("三方：5127681781169041222").as_deref(), Some("5127681781169041222"));
    assert_eq!(hit("18187841287  客户三个安装单换个师傅"), None, "11 位手机号不能当订单号");
    assert_eq!(hit("客户电话 18187841287 打不通"), None, "手机号在正文中间也不能当订单号");
    assert_eq!(hit("加14个筒灯"), None);
    assert_eq!(hit("6954604 下个保护拆"), None, "6 位数字不是单号");
}

// ─────────────────────────────────────────────────────────────────────────────
// _body —— 错一条就是 PII 明文出境，或者订单号被掩掉半截
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
        assert!(PLACEHOLDER.is_match(m), "validator 拦不住 summary 里的「{m}」");
    }
    // 反向：body 真的产出这三个（不是只在文档里一致）。
    // ⚠️ 手机号必须放在**字段锚点之外** —— `客户电话:138…` 会被 FIELD 整段掩成
    //    `<略>`，`<手机号>` 根本轮不到出场（顺序是承重的，见 ADR-0001）。
    let s = b("@李培尚 打不通 13581496310 / 客户姓名：张三");
    for m in [MASK_PHONE, MASK_FIELD, MASK_AT] {
        assert!(s.contains(m), "body 没产出「{m}」：{s}");
    }
}

#[test]
fn body_deletes_the_quote_block_with_the_name_and_phone_inside_it() {
    let q = "\"王鸿江：\nJDLY202606271814212465\n\n\n安排师傅去换配件，\
             客户电话：17379865588，费用算我们\"\n------\n单子已经安排好了哦@王鸿江  ";
    assert_eq!(b(q), "单子已经安排好了哦@某人");
    assert!(!b(q).contains("王鸿江") && !b(q).contains("17379865588"), "引用块里的姓名/手机号漏出去了");
}

#[test]
fn body_replaces_at_mentions_including_the_parenthesised_note() {
    assert_eq!(b("@李晶  "), "@某人");
    assert_eq!(b("@丁家乐  @丁家乐  "), "@某人 @某人");
    assert_eq!(b("@李培尚(李培尚-东区销售部-售后客服)  加三四十"), "@某人 加三四十");
}

#[test]
fn body_masks_phones_including_the_extension_suffix() {
    assert_eq!(b("这个号码，让师傅联系处理一下18903170081"), "这个号码，让师傅联系处理一下<手机号>");
    assert_eq!(b("手机号:18472625055-3934"), "手机号:<手机号>");
    // 师傅姓名不动 —— 只有手机号被掩（姓名规则已删，ADR-0001）
    assert_eq!(b("转给这个师傅：杨师傅 13289149875"), "转给这个师傅：杨师傅 <手机号>");
}

/// ADR-0001:36 那 363 处差额。手写的两侧断言就是为了这一条。
#[test]
fn phone_masking_never_touches_a_single_character_of_an_order_number() {
    for o in [
        "5127366458053009229",
        "JDLY202608031734008496",
        "3316977912130066680",
        "3593403004800240",
        "1836102489781612345",
    ] {
        let got = b(&format!("{o} 加14个筒灯"));
        assert_eq!(got, format!("{o} 加14个筒灯"), "订单号被动了");
        assert_eq!(ORDER_NO.find(&got).unwrap().as_str(), o, "掩码后订单号提不出来了");
    }
    assert_eq!(
        ORDER_NO.find(&b("5127366458053009229  淘宝 维修 / 袁柳，13581496310")).unwrap().as_str(),
        "5127366458053009229",
        "脱敏动了订单号"
    );
}

#[test]
fn body_folds_every_newline_so_one_message_is_always_one_line() {
    // 正文冒充行框架是承重不变量 6（溯源）的绕过路径
    let addr = "3298291251974226652  淘宝 维修\n\n王小宾，15836102489-7818，河南省 新乡市";
    assert_eq!(b(addr), "3298291251974226652  淘宝 维修 / 王小宾，<手机号>，河南省 新乡市");
    assert!(!b(addr).contains('\n') && !b(addr).contains('\r'), "折行没折干净");

    let ms = msgs(200);
    assert!(
        ms.iter().all(|m| !body(m).contains('\n') && !body(m).contains('\r')),
        "有正文仍是多行"
    );
}

#[test]
fn body_does_not_truncate() {
    assert_eq!(b(&"啊".repeat(254)).chars().count(), 254, "不该截断");
}

#[test]
fn body_masks_anchored_fields_only_and_keeps_the_separator_spacing() {
    assert_eq!(
        b("客户:栗子 / 手机号:13581496310 / 地址:湖南省长沙市天心区花语江南7栋1303"),
        "客户:<略> / 手机号:<手机号> / 地址:<略>",
        "「手机号」不在 _FIELD 表里 —— 它的值由 PHONE 掩成 <手机号>，不是 <略>"
    );
    // 分隔符两侧的空格必须留着 —— 值换成 <略> 之后字段名和上一个值不能糊在一起
    assert_eq!(b("客户:栗子 / 地址:河南新乡"), "客户:<略> / 地址:<略>");
    // 长字段名排在短的前面，否则「客户姓名：贾世强」只吃掉「客户」二字
    assert!(!b("客户姓名：贾世强").contains("贾世强"), "长字段名没优先匹配，姓名漏出去了");
}

/// 删掉姓名规则的全部理由：中文里动作和人名字形完全一样，掩错就把一个动作变成一个人。
#[test]
fn body_rewrites_zero_business_verbs() {
    for verb in ["改电话", "指派", "转师傅", "改", "电话", "改地址"] {
        let got = b(&format!("{verb} 13581496310"));
        assert_eq!(got, format!("{verb} <手机号>"), "业务动词被改写了");
    }
    // 自由文本里的姓名/地址原样保留 —— 有意为之，不是漏了
    assert_eq!(
        b("王小宾，15836102489，河南省 新乡市 牧野区 大桥云锦府20号楼302"),
        "王小宾，<手机号>，河南省 新乡市 牧野区 大桥云锦府20号楼302"
    );
    assert_eq!(b("指派 孙师傅 13581496310"), "指派 孙师傅 <手机号>");
}

// ─────────────────────────────────────────────────────────────────────────────
// summary 校验 —— 它归事实列，PII 一旦进去就是永久的，缓存还会把它焊死
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn summary_validation_blocks_ids_phones_placeholders_and_overlength() {
    let bad = |s: &str| {
        validate(
            vec![EventDraft {
                r#ref: None,
                msg_indexes: vec![1],
                summary: s.into(),
                still_open: true,
            }],
            400,
            &BTreeSet::new(),
        )
        .err()
    };
    assert!(bad("商家要求加单，平台已受理").is_none(), "正常 summary 被误拒");
    assert!(bad("5127366458053009229 要求加单").unwrap().contains("订单号"));
    assert!(bad("客户18472625055要求改期").unwrap().contains("手机号"));
    for ph in ["商家发来<手机号>", "客户信息<略>", "回复@某人"] {
        assert!(bad(ph).unwrap().contains("占位符"), "占位符没挡住: {ph}");
    }
    // 长度按 Unicode 码点，不是字节 —— 101 个汉字是 303 字节
    assert!(bad(&"啊".repeat(101)).unwrap().contains("超过 100 字"));
    assert!(bad(&"啊".repeat(100)).is_none(), "刚好 100 字该放行");
}

#[test]
fn validation_rejects_out_of_range_indexes_and_unknown_refs() {
    let ev = |r: Option<u32>, ix: Vec<usize>| {
        vec![EventDraft { r#ref: r, msg_indexes: ix, summary: "正常".into(), still_open: true }]
    };
    let refs: BTreeSet<u32> = [2u32].into_iter().collect();

    assert!(validate(ev(None, vec![0]), 10, &refs).unwrap_err().contains("超出本段范围 1-10"));
    assert!(validate(ev(None, vec![11]), 10, &refs).unwrap_err().contains("超出本段范围 1-10"));
    assert!(validate(ev(Some(5), vec![1]), 10, &refs).unwrap_err().contains("E5 不在"));
    assert!(validate(ev(Some(2), vec![1]), 10, &refs).is_ok(), "便签上有的 ref 该放行");

    // 去重 + 排序是契约不是顺手
    let ok = validate(ev(None, vec![3, 1, 3]), 10, &refs).unwrap();
    assert_eq!(ok[0].msg_indexes, [1, 3]);
}

// ─────────────────────────────────────────────────────────────────────────────
// ④ 装配 —— 承重不变量 6 的守卫
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn assemble_computes_every_field_from_real_messages() {
    let ms = msgs(10);
    // 下标 1(EXTERNAL) 发起，2(INTERNAL) 首响，4(INTERNAL) 也参与
    let ev = assemble(&draft(&[1, 2, 4], "商家要求加单，平台已受理", false), &ms).unwrap();
    assert_eq!(ev.source_msg_ids, ["m00001", "m00002", "m00004"]);
    assert_eq!(ev.asker, ms[1].sender_id);
    assert_eq!(ev.asker_role, Role::External);
    assert_eq!(ev.first_msg_time, ms[1].at);
    assert_eq!(ev.last_msg_time, ms[4].at);
    assert_eq!(ev.first_agent_reply_time, Some(ms[2].at));
    assert_eq!(ev.first_responder.as_deref(), Some(ms[2].sender_id.as_str()));
    assert_eq!(ev.occurred_on, ms[1].at.date());
    assert!(ev.agents.contains(&ms[2].sender_id), "首响人必须在 agents 里");
}

#[test]
fn assemble_leaves_first_response_null_when_no_platform_replied() {
    let ms = msgs(10);
    let ev = assemble(&draft(&[1, 3], "商家提了个要求，没人回", true), &ms).unwrap();
    assert!(ev.first_agent_reply_time.is_none() && ev.first_responder.is_none());
    assert!(ev.agents.is_empty(), "没有平台回复就不该有 agents");
}

#[test]
fn assemble_refuses_an_empty_provenance() {
    let e = assemble(&draft(&[], "无来源", true), &msgs(10)).unwrap_err();
    assert!(e.to_string().contains("承重不变量 6"), "{e}");
}

#[test]
fn assemble_refuses_an_overlong_summary() {
    let e = assemble(&draft(&[1], &"啊".repeat(101), true), &msgs(10)).unwrap_err();
    assert!(e.to_string().contains("summary 超长"), "{e}");
}

#[test]
fn assemble_refuses_inverted_times() {
    // idx 逆序 -> first > last。正常路径上 idx 恒有序，这条守的是「万一无序」
    let e = assemble(&draft(&[5, 1], "时间倒挂", true), &msgs(10)).unwrap_err();
    assert!(e.to_string().contains("时间倒挂"), "{e}");
}

// ─────────────────────────────────────────────────────────────────────────────
// 孤儿哨兵
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn orphans_counts_platform_started_events_without_an_order_number() {
    let mut ms = msgs(10);
    ms[0].sender_role = Role::Internal;
    ms[2].sender_role = Role::Internal;
    ms[2].text = "工单原因：电话核实 订单号:JDLY202608031734008496".into();
    let drafts: BTreeMap<u32, Draft> = [
        (1u32, draft(&[0], "被撕下来的应答尾巴", false)), // INTERNAL 起头、无单号 -> 孤儿
        (2u32, draft(&[2], "平台推的工单", false)),        // INTERNAL 起头、有单号 -> 不是
        (3u32, draft(&[1], "商家发起", false)),            // EXTERNAL 起头 -> 不是
    ]
    .into();
    assert_eq!(orphans(&drafts, &ms), 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// 端到端：走真实的 ①② 读取路径，接上桩模型
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn extract_runs_end_to_end_on_a_room_read_through_ingest() {
    let root = testutil::fresh_root("extract", "e2e");
    let base = day().and_hms_opt(9, 0, 0).unwrap().and_utc().timestamp_millis()
        - 8 * 3600 * 1000; // 业务本地时区 -> UTC 毫秒
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

    let events = extract(&conv.msgs, &BisectStub::new(usize::MAX), 15).await.unwrap();
    // 桩每段出一个事件，40 条 / 段长 15 -> 3 段
    assert_eq!(events.len(), 3);
    assert!(events.iter().all(|e| !e.source_msg_ids.is_empty()), "承重不变量 6：溯源非空");
    let known: BTreeSet<&str> = conv.msgs.iter().map(|m| m.msg_id.as_str()).collect();
    assert!(
        events.iter().flat_map(|e| &e.source_msg_ids).all(|id| known.contains(id.as_str())),
        "溯源 ID 必须真实存在于该次抽取的消息里"
    );
    assert!(events.iter().all(|e| e.occurred_on == day()));
}

#[tokio::test]
async fn extract_on_an_empty_conversation_is_ok_not_an_error() {
    // 承重不变量 4：Ok([]) 与 Failed 绝不混淆
    let events = extract(&[], &BisectStub::new(0), 400).await.unwrap();
    assert!(events.is_empty());
}
