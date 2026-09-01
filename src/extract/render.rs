//! 渲染链 —— [`view`] 是「模型这一段看到什么」的**唯一出口**。
//!
//! 便签淘汰（[`note`]）· 角色化匿名标签（[`labels`]）· 行号与回复箭头（[`render`]）
//! 三件事必须彼此一致，由 [`view`] 的构造保证（见那里的三条）。
//!
//! **模型在这里看不到 `msg_id`，也看不到 `easyUserId`** —— 给的是段内 1-based 序号，
//! 越界即校验失败（承重不变量 6）。

use super::{
    Draft, NOTE_QUOTE,
    redact::{ORDER_NO, body},
};
use crate::ingest::{Message, Role};
use std::collections::{BTreeMap, BTreeSet};

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
        let n = if is_internal { &mut internal } else { &mut external };
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

/// 取前 n 个**字符**（不是字节）—— 便签原话的截断。
pub(super) fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
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
/// **撤下便签 ≠ 丢掉事件** —— draft 还在 `drafts` 里照样被 [`assemble`] 输出，撤的只是
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
        let replied = d.idx.iter().any(|&i| replied_to.contains(msgs[i].msg_id.as_str()));
        if stale && !replied {
            continue;
        }
        let head = d
            .idx
            .iter()
            .find_map(|&i| ORDER_NO.find(&body(&msgs[i])).map(|m| m.as_str().to_string()));
        let summary = match head {
            Some(h) => format!("{h} · {}", d.summary),
            None => d.summary.clone(),
        };
        out.push((r, summary, head_chars(&body(&msgs[last_idx]), NOTE_QUOTE)));
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
                format!("「{}」", head_chars(&body(m), NOTE_QUOTE))
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
