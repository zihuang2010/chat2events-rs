//! 渲染链 —— [`view`] 是「模型这一段看到什么」的**唯一出口**。
//!
//! 便签淘汰（[`note`]）· 角色化匿名标签（[`labels`]）· 行号与回复箭头（[`render`]）
//! 三件事必须彼此一致，由 [`view`] 的构造保证（见那里的三条）。
//!
//! **模型在这里看不到 `msg_id`，也看不到 `easyUserId`** —— 给的是段内 1-based 序号，
//! 越界即校验失败（承重不变量 6）。

use super::{
    redact::{ORDER_NO, body},
    types::Draft,
};
use crate::ingest::{Message, Role};
use std::collections::{BTreeMap, BTreeSet};

/// 便签里带的原话截断长度（**字符数**不是字节，`chars().take` 保证）。
/// 只有本文件消费它（便签原话 · 段外引用的原话退化），所以住在这里。
const NOTE_QUOTE: usize = 40;

/// 角色化匿名标签：`sender_id -> 平台A / 商家B`。
///
/// **必须按整群消息算一次，不能按段算。** 两边都是客服：`INTERNAL` 是平台客服，
/// `EXTERNAL` 是商家客服，没有终端消费者（标成「客服 / 客户」会把模型带进消费者支持
/// 的思路，而这是 B2B 派单）。
///
/// 按段算实测（1096 条切 3 段）：**32 处标签冲突**（段 1 的「平台B」和段 2 的「平台B」
/// 是两个不同的人）、**30 处身份漂移**（同一个人从「平台C」变成「平台F」），二分时
/// 每一半还会再洗一次。便签花大力气把跨段事件接住，接住之后模型看到的却是一套洗过牌
/// 的角色表 —— 等于在最需要连贯的地方引入了不连贯。样本 23 个发言人，A–Z 装得下。
pub(super) fn labels(msgs: &[Message]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let (mut internal, mut external) = (0usize, 0usize);
    for m in msgs {
        if out.contains_key(&m.sender_id) {
            continue;
        }
        let is_internal = m.sender_role == Role::Internal;
        let n = if is_internal {
            &mut internal
        } else {
            &mut external
        };
        let k = *n;
        *n += 1;
        let prefix = if is_internal { "平台" } else { "商家" };
        let letter = (b'A' + (k % 26) as u8) as char;
        let suffix = if k / 26 == 0 {
            String::new()
        } else {
            (k / 26).to_string()
        };
        out.insert(m.sender_id.clone(), format!("{prefix}{letter}{suffix}"));
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// 渲染链 —— `view` 是「模型这一段看到什么」的唯一出口
// ─────────────────────────────────────────────────────────────────────────────

/// 便签：还开着**且还可能被接着说**的事件，每条 = `(编号, 摘要, 最后一条来源消息的原话)`。
///
/// 摘要是压缩过的（「等师傅上门」），而下一段开头的灰消息（「好了谢谢」）要跟原话对齐
/// 才好判断归属 —— 所以带上原话。这就是「只读的重叠」。
///
/// **`keep` 传的是段长**，所以淘汰判据读作「**上一整段都没动静就撤下**」——
/// 不发明第二个数。根因是 `still_open` 只在模型这一段又提到该事件时才更新，没提到就
/// 永远开着，而派单群里「客服说安排了、然后没下文」是常态。3742 条实测便签峰值 243 条。
///
/// **例外：本段有人显式 `replyTo` 它就捞回来，多远都捞**（实测最远回指 947 条）。
///
/// **撤下便签 ≠ 丢掉事件** —— draft 还在 `drafts` 里照样被 `super::assemble` 输出，撤的只是
/// 「拿给模型看的那一份」。这是注意力问题，不是存储问题。
///
/// **便签上必须带订单号**：`summary` 按契约不含 ID，可订单号正是这个群唯一可靠的关联键
/// （误合并率 0.1%，而人员 33%、时间 ±30min 26%）。不带的话模型看到「E7: 客户要求加急」，
/// 根本无从判断本段那条同单号的消息是不是接着它。**只是摆给模型看，关联仍由模型做** ——
/// 代码不替它 join（同一单号下常有好几件互不相干的事，ADR-0004）。
pub(super) fn note(
    drafts: &BTreeMap<u32, Draft>,
    msgs: &[Message],
    lo: usize,
    hi: usize,
    keep: usize,
) -> Vec<(u32, String, String)> {
    let replied_to: BTreeSet<&str> = msgs[lo..hi]
        .iter()
        .filter_map(|m| m.reply_to.as_deref())
        .collect();
    let mut out = Vec::new();
    for (&r, d) in drafts {
        if !d.still_open {
            continue;
        }
        let last_idx = *d.idx.last().expect("drafts 里不存 idx 为空的 draft");
        // saturating：二分时后一半的 lo 恒 > 前一半 draft 的 idx，正常不会倒过来；
        // 真倒过来说明这个 draft 刚被本段碰过，语义上就是「有动静」，不该撤。
        let stale = lo.saturating_sub(last_idx) > keep;
        let replied = d
            .idx
            .iter()
            .any(|&i| replied_to.contains(msgs[i].msg_id.as_str()));
        if stale && !replied {
            continue;
        }
        let head = d.idx.iter().find_map(|&i| {
            ORDER_NO
                .find(&body(&msgs[i]))
                .map(|m| m.as_str().to_string())
        });
        let summary = match head {
            Some(h) => format!("{h} · {}", d.summary),
            None => d.summary.clone(),
        };
        out.push((
            r,
            summary,
            body(&msgs[last_idx]).chars().take(NOTE_QUOTE).collect(),
        ));
    }
    out
}

/// 渲染成模型看到的文本：便签 + 行号 + 角色化匿名标签 + 回复箭头。
///
/// `labels` 由调用方按**整群**算好传进来（见 [`labels`]）—— 不在这里算，因为这里只
/// 看得见一段，按段算就会让同一个标签在不同段指向不同的人。
///
/// 模型看不到 `msg_id`，也看不到 `easyUserId` —— 序号越界即校验失败，从根本上消灭
/// 「模型编造 msgid」这个失败模式（承重不变量 6）。
///
/// **段外的引用一条都不能静默丢掉** —— 实测丢的恰好是回指最远那批（128~947 条），
/// 也正是模型唯一没法从内容猜出来的那批。所以箭头有三种，由 `outside` 决定后两种：
///
/// ```text
/// ↩回复 #12          指本段第 12 行
/// ↩回复 E7           指段外，且目标挂在便签 E7 上 —— 模型可以接上去
/// ↩回复「明天上门」   指段外，目标不在便签上（灰消息 / 已闭合的事）——
///                    只给信息，**不给可接的 ref**，让模型自己判断要不要开新事件
/// ```
pub(super) fn render(
    seg: &[Message],
    labels: &BTreeMap<String, String>,
    note: &[(u32, String, String)],
    outside: &BTreeMap<String, String>,
) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(seg.len() + note.len() + 3);
    if !note.is_empty() {
        lines.push("【进行中的事件】（上一段留下的，本段接着说就填对应 ref）".into());
        for (r, s, q) in note {
            lines.push(format!("E{r}: {s} ← 上次说到「{q}」"));
        }
        lines.push(String::new());
        lines.push("【本段消息】".into());
    }
    let index_of: BTreeMap<&str, usize> = seg
        .iter()
        .enumerate()
        .map(|(i, m)| (m.msg_id.as_str(), i + 1))
        .collect();
    for (i, m) in seg.iter().enumerate() {
        let arrow = match m.reply_to.as_deref() {
            Some(rt) if index_of.contains_key(rt) => format!(" ↩回复 #{}", index_of[rt]),
            Some(rt) if outside.contains_key(rt) => format!(" ↩回复 {}", outside[rt]),
            _ => String::new(),
        };
        lines.push(format!(
            "#{} [{}] {}{}: {}",
            i + 1,
            m.at.format("%H:%M:%S"),
            labels[&m.sender_id],
            arrow,
            body(m)
        ));
    }
    lines.join("\n")
}

/// 模型这一段看到的全部内容 + validator 该认哪些 ref。**唯一的出口。**
///
/// ⚠️ **`labels` 在这里按整群现算，每段一次** —— 看着像重复劳动，实测不是：
/// 3742 条 / 10 段跑完整条渲染链（release）共 **6.3ms**，labels 只占其中一小块，
/// 而同一个群的 10 次模型调用是分钟级。把它提到 `extract` 里算一次要给
/// `run` / `one_call` / `view` 三个签名各加一个参数，换回来的是几毫秒 —— 不换。
/// （正确性上两者恒等：labels 读的是整群 `msgs`，与 `lo`/`hi` 无关。）
///
/// 便签淘汰、段外引用解析、渲染、`open_refs` 这四件事必须彼此一致，**由构造保证**：
///   * `open_refs` 必须等于便签的 ref 集合（否则模型接了个合法 ref 却被判编造）
///   * `outside` 的 `E<ref>` 必须只来自便签（否则给出去的 ref validator 不认）
///   * `segment_size` 必须等于 `hi - lo`（否则越界检查是错的）
pub(super) fn view(
    msgs: &[Message],
    lo: usize,
    hi: usize,
    drafts: &BTreeMap<u32, Draft>,
    segment_msgs: usize,
) -> (String, BTreeSet<u32>) {
    let seg = &msgs[lo..hi];
    // 便签的保留窗口 == 段长：往回看一段，正好是「上一整段都没动静就撤下」。
    let note_rows = note(drafts, msgs, lo, hi, segment_msgs);

    let in_seg: BTreeSet<&str> = seg.iter().map(|m| m.msg_id.as_str()).collect();
    let want: BTreeSet<&str> = seg
        .iter()
        .filter_map(|m| m.reply_to.as_deref())
        .filter(|rt| !in_seg.contains(rt))
        .collect();

    // 挂在便签上的给 E<ref>（模型可以接上去），其余的给一句原话。**不给 ref 是有意的** ——
    // 便签上没有的 ref 会被 validator 当成编造，而「显式回复就重开已闭合的事」正对着
    // 「一条群公告被回 20 次」的过度合并风险，宁可让它开一个新事件。
    let mut on_note: BTreeMap<&str, String> = BTreeMap::new();
    for (r, _, _) in &note_rows {
        for &i in &drafts[r].idx {
            on_note.insert(msgs[i].msg_id.as_str(), format!("E{r}"));
        }
    }
    let outside: BTreeMap<String, String> = msgs
        .iter()
        .filter(|m| want.contains(m.msg_id.as_str()))
        .map(|m| {
            let tag = on_note.get(m.msg_id.as_str()).cloned().unwrap_or_else(|| {
                format!(
                    "「{}」",
                    body(m).chars().take(NOTE_QUOTE).collect::<String>()
                )
            });
            (m.msg_id.clone(), tag)
        })
        .collect();

    if !want.is_empty() {
        let linkable = outside.values().filter(|v| !v.starts_with('「')).count();
        // 指向本次会话之外的（跨文件/跨天，实测 265 条 replyTo 里有 7 条）既渲染不出箭头
        // 也接不上便签。早先这里打的是 outside.len()，那 7 条连计数都没有 —— 正是本模块
        // 反复要消灭的那类静默丢失。顺带它就是回看窗口 N 的判据数据。
        let lost = want.len() - outside.len();
        tracing::info!(lo, hi, want = want.len(), linkable, lost, "[段外引用]");
    }
    // 带进去 / 还开着。差额就是「上一整段没动静」被撤下的 —— 撤多撤少都不能是静默的。
    let open_now = drafts.values().filter(|d| d.still_open).count();
    if open_now > 0 {
        tracing::info!(lo, hi, carried = note_rows.len(), open = open_now, "[便签]");
    }

    let refs = note_rows.iter().map(|(r, _, _)| *r).collect();
    (render(seg, &labels(msgs), &note_rows, &outside), refs)
}

#[cfg(test)]
mod tests {
    use super::super::{
        segment::segments,
        tests::{SEG, draft, msgs},
    };
    use super::*;

    #[test]
    fn labels_are_computed_once_per_room_and_never_collide() {
        let ms = msgs(200);
        let all = labels(&ms);
        let distinct: BTreeSet<&String> = all.values().collect();
        assert_eq!(distinct.len(), all.len(), "标签撞号 —— 两个人共用一个标签");
        assert!(
            all.values()
                .all(|v| v.starts_with("平台") || v.starts_with("商家"))
        );

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
        assert!(
            !render(&seg, &l, &tag, &e7).contains("↩回复"),
            "没人引用时不该冒出箭头"
        );

        let mut far = seg.clone();
        far[3].reply_to = Some(far_id.clone()); // 指到段外
        assert!(
            render(&far, &l, &tag, &e7).contains("↩回复 E7"),
            "段外 replyTo 的箭头被丢了"
        );
        assert!(
            render(&far, &l, &tag, &quoted).contains("↩回复 「原话」"),
            "便签上没有的该给原话"
        );
        assert!(
            !render(&far, &l, &tag, &BTreeMap::new()).contains("↩回复"),
            "outside 没给映射时不能凭空造箭头"
        );

        let mut near = seg.clone();
        near[5].reply_to = Some(seg[1].msg_id.clone());
        assert!(
            render(&near, &l, &[], &BTreeMap::new()).contains("↩回复 #2"),
            "段内 replyTo 没渲染"
        );
    }

    #[test]
    fn view_keeps_open_refs_identical_to_the_note() {
        let mut ms = msgs(700);
        let far_id = ms[10].msg_id.clone();
        ms[605].reply_to = Some(far_id); // 指到段外、且挂在便签上

        let probe: BTreeMap<u32, Draft> = [(7u32, draft(&[10], "远处那件事", true))].into();
        let (text, open_refs) = view(&ms, 600, 610, &probe, SEG);
        assert_eq!(
            open_refs,
            [7u32].into_iter().collect::<BTreeSet<_>>(),
            "open_refs 与便签不一致"
        );
        assert!(
            text.contains("E7:") && text.contains("↩回复 E7"),
            "便签上的段外引用没接上"
        );

        let (text2, open2) = view(&ms, 600, 610, &BTreeMap::new(), SEG);
        assert!(open2.is_empty(), "没有便签时 open_refs 必须为空");
        assert!(
            text2.contains("↩回复 「"),
            "便签外的段外引用该退化成原话，不能静默丢"
        );
        assert!(!text2.contains("【进行中的事件】"), "空便签不该渲染出标题");
    }

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
            note(&probe, m, 600, 700, keep)
                .into_iter()
                .map(|(r, _, _)| r)
                .collect()
        };
        assert_eq!(
            kept(&ms, 1000),
            [1, 2],
            "窗口够大时不该撤，闭合的本来就不该进"
        );
        assert_eq!(kept(&ms, 200), [1], "窗口外的没撤下去");

        let mut pulled = ms.clone();
        pulled[650].reply_to = Some(ms[10].msg_id.clone()); // 本段显式引用那件远事
        assert_eq!(
            kept(&pulled, 200),
            [1, 2],
            "被 replyTo 指到的必须捞回来，多远都捞"
        );
    }

    #[test]
    fn note_carries_the_order_number_as_the_only_reliable_join_key() {
        let mut ms = msgs(700);
        ms[10].text = "5127366458053009229  加14个筒灯".into();
        let probe: BTreeMap<u32, Draft> = [(1u32, draft(&[10], "商家要求加单", true))].into();
        let rows = note(&probe, &ms, 600, 700, 10_000);
        assert_eq!(
            rows[0].1, "5127366458053009229 · 商家要求加单",
            "便签没带上订单号"
        );
    }
}
