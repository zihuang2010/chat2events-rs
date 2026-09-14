//! ④ 装配 —— 段输出 → 全群 draft → [`Event`]。
//!
//! **明确不给端口**：它是承重不变量 6（溯源）的守卫，给它接缝等于给溯源留绕过口。
//!
//! 四件事按调用顺序住在这里：
//!   * [`merge`] —— 段内行号 → 全局下标的换算，**唯一发生地**（承重不变量 6）；
//!   * [`align`] —— 显式 `replyTo` 跨 draft 就并（钥匙只有 `replyTo`）；
//!   * [`assemble`] —— 除 `summary` 外每个字段都从真实消息算，承重校验就在那里；
//!   * [`orphans`] —— 模型分组质量的哨兵，**只打不改**。
//!
//! ⚠️ **这个文件 500 多行，明确不拆。** ④ 是承重不变量 6（溯源）的守卫，
//! `docs/architecture.md` 写着「④ 明确不给端口 —— 给它接缝等于给溯源留绕过口」。
//! 按职责拆成兄弟文件会在 ④ 内部造出一条 `pub(super)` 边界，正是那条设计要避免的
//! 东西。（唯一可议的是 `orphans` —— 它是模型质量哨兵、只打日志不改数据，
//! 严格说不属于装配；但它 21 行，单独一个文件更差。）

use super::{
    redact::{ORDER_NO, body},
    types::{Draft, Event, EventDraft, SUMMARY_MAX, SourceMessage},
};
use crate::{
    BoxError,
    stage::ingest::{Message, Role},
    worktime,
};
use chrono::NaiveDateTime;
use std::collections::BTreeMap;

/// 模型这一段的输出并进全群 `drafts`。**段内行号 → 全局下标的换算就在这里，仅此一处。**
///
/// **`summary` 取首个，不覆盖。** 与 `asker` / `first_msg_time` / `occurred_on` 同源，
/// 也与 [`align`] 合并时「保留 `idx[0]` 最小那个的 summary」是同一条规则。早先这里是
/// 无条件覆盖：同一件事被切开，断在段边界就拿到尾部状态（「已取消完毕」），断在段内经
/// `align` 就拿到诉求（「要求取消」）—— **按它在哪儿被拆开会得到不同的 type**，而决定
/// type 的是诉求不是结果状态。
///
/// `still_open` 相反，**必须取最新** —— 它是状态标志，不是事实。
pub(super) fn merge(drafts: &mut BTreeMap<u32, Draft>, events: Vec<EventDraft>, lo: usize) {
    for ev in events {
        // 「`Draft.idx` 恒非空」的守卫在唯一的生产点（`validate` 已拒绝空 `msg_indexes`，
        // 到这里不可能为假）—— `render` 的 expect、`align` / `orphans` 的裸下标全依赖它。
        assert!(
            !ev.msg_indexes.is_empty(),
            "validate 拒绝空 msg_indexes，到这里恒非空"
        );
        let r = ev
            .r#ref
            .unwrap_or_else(|| drafts.keys().next_back().copied().unwrap_or(0) + 1);
        let d = drafts.entry(r).or_default();
        d.idx.extend(ev.msg_indexes.iter().map(|i| lo + i - 1));
        d.idx.sort_unstable();
        d.idx.dedup();
        if d.summary.is_empty() {
            d.summary = ev.summary;
        }
        d.still_open = ev.still_open;
    }
}

/// 显式 `replyTo` 跨 draft 就并 —— 那是**发消息的人自己标的归属**，比模型的判断可靠。
///
/// 1096 条实测：258 条带 `replyTo` 的消息里 **33 条被模型拆到了两个事件**，由此产生
/// 42 个 `asker=平台` 的畸形事件（首响时效全是 0 秒，指标被直接污染）。并回之后
/// 326 → 289 个事件，畸形 42 → 24（剩下的是合法的平台工单推送）。
///
/// 合并时**保留 `idx[0]` 最小那个 draft 的 summary** —— 与 `merge` 同一条规则。
///
/// ⚠️ **钥匙只有 `replyTo`，绝不是订单号。** 同一个单号下可以有好几件互不相干的事
/// （实测同单号且间隔 0 行的两个事件，一个「加同款保护拆」一个「加灯具维修」）——
/// 订单号是「工单」的钥匙不是「事件」的钥匙。
///
/// 全群跑一次，不逐段跑：**断开在段内，不在段边界**（便签已经把跨段那几个接住了）。
pub(super) fn align(drafts: BTreeMap<u32, Draft>, msgs: &[Message]) -> BTreeMap<u32, Draft> {
    let mut where_: BTreeMap<usize, u32> = BTreeMap::new();
    for (&r, d) in &drafts {
        for &i in &d.idx {
            where_.insert(i, r);
        }
    }
    let pos: BTreeMap<&str, usize> = msgs
        .iter()
        .enumerate()
        .map(|(i, m)| (m.msg_id.as_str(), i))
        .collect();
    let mut par: BTreeMap<u32, u32> = drafts.keys().map(|&r| (r, r)).collect();

    fn find(par: &mut BTreeMap<u32, u32>, mut x: u32) -> u32 {
        while par[&x] != x {
            let g = par[&par[&x]];
            par.insert(x, g); // 路径减半
            x = g;
        }
        x
    }

    let mut pairs = 0usize;
    for m in msgs {
        let Some(rt) = m.reply_to.as_deref() else {
            continue;
        };
        let Some(&j) = pos.get(rt) else { continue };
        let (Some(&a), Some(&b)) = (where_.get(&pos[m.msg_id.as_str()]), where_.get(&j)) else {
            continue;
        };
        let (ra, rb) = (find(&mut par, a), find(&mut par, b));
        if ra == rb {
            continue;
        }
        par.insert(ra, rb);
        pairs += 1;
    }

    let mut groups: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for &r in drafts.keys() {
        groups.entry(find(&mut par, r)).or_default().push(r);
    }
    let mut out = BTreeMap::new();
    for members in groups.values() {
        let keep = *members
            .iter()
            .min_by_key(|r| drafts[r].idx[0])
            .expect("并查集的每个组至少有一个成员");
        let mut idx: Vec<usize> = members.iter().flat_map(|r| drafts[r].idx.clone()).collect();
        idx.sort_unstable();
        idx.dedup();
        out.insert(
            keep,
            Draft {
                idx,
                ..drafts[&keep].clone()
            },
        );
    }
    // 静默的合并等于不知道自己的事件在被合 —— 和 [切分] / [便签] 同一条纪律
    if pairs > 0 {
        tracing::info!(pairs, before = drafts.len(), after = out.len(), "[对齐]");
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// ④ 装配 —— **明确不给端口**，它是承重不变量 6（溯源）的守卫
// ─────────────────────────────────────────────────────────────────────────────

/// 后续轮次最长等待 —— **首响之后**每次「商家说话 → 客服接话」的工作时段间隔取最大。
///
/// 工作时段口径住在 [`crate::worktime`]。它此前是本文件的私有常量，因为当时**只有
/// 这一列**走工作时段；首响改成同一口径之后 ⑥ 和 webUI 也要用，就搬走了。
///
/// 从首条 `INTERNAL` 的**下一条**起扫，首响那一次于是天然不在内：它已经是
/// `first_agent_reply_time - first_msg_time`，再算一遍就是重复计数
/// （两者如今同一口径，更不该叠）。
/// 平台发起的事件（首条就是 `INTERNAL`）同理 —— 它的第一次 EXTERNAL→INTERNAL
/// 是真实的后续等待，照算不跳。
///
/// **末尾没人接的那一段不计**：它没有终点，算出来的是「跑批那一刻」而不是事实，
/// 而「把商家晾着」本来就是 `last_msg_role` 的活。
///
/// 取 max 不取 avg：avg 会被长事件稀释，而能落到人头上的是「最长被晾多久」。
///
/// 返回 `0` = **确实没有后续轮次**，是算出来的事实；「没算过」是列上的 NULL。
/// 两者不混（承重不变量 4）。
fn followup_wait_max_sec(src: &[&Message]) -> u32 {
    let Some(first_reply) = src.iter().position(|m| m.sender_role == Role::Internal) else {
        return 0;
    };
    let (mut max, mut waiting) = (0i64, None::<NaiveDateTime>);
    for m in &src[first_reply + 1..] {
        match (m.sender_role, waiting) {
            (Role::External, None) => waiting = Some(m.at),
            (Role::Internal, Some(since)) => {
                waiting = None;
                max = max.max(worktime::between(since, m.at));
            }
            _ => {}
        }
    }
    u32::try_from(max).unwrap_or(0)
}

/// 除 `summary` 外的每一个字段都在这里从真实消息算出。
///
/// 承重不变量住在这里，**不在调用方** —— 「模型输出必须先校验再落库」，而调用方会忘记调。
/// 只留承重的几条：`occurred_on` 的定义在本函数里构造上恒真，不重复校验。
///
/// ⚠️ **一律 `Result` 不用 `panic!`**（`CLAUDE.md` 的 Rust 规矩）：
/// `assert!` 在 release 下不会消失，但它会掀掉整轮 —— 而承重不变量 3 要求「某个群失败
/// → 该群跳过一行不写，**整轮继续**」。所以群级失败一律走 `Result`。
pub(super) fn assemble(d: &Draft, msgs: &[Message]) -> Result<Event, BoxError> {
    if d.idx.is_empty() {
        return Err("source_msg_ids 必须非空（承重不变量 6：溯源）".into());
    }
    let src: Vec<&Message> = d.idx.iter().map(|&i| &msgs[i]).collect();
    let internal: Vec<&&Message> = src
        .iter()
        .filter(|m| m.sender_role == Role::Internal)
        .collect();
    let first_reply = internal.first().copied();

    // agents 是**插入序去重**，不是排序 —— 顺序即出场顺序。
    let mut agents: Vec<String> = Vec::new();
    for m in &internal {
        if !agents.iter().any(|a| a == &m.sender_id) {
            agents.push(m.sender_id.clone());
        }
    }

    let ev = Event {
        corpid: src[0].corp.clone(),
        roomid: src[0].room.clone(),
        source_msg_ids: src.iter().map(|m| m.msg_id.clone()).collect(),
        first_msg_time: src[0].at,
        last_msg_time: src[src.len() - 1].at,
        first_agent_reply_time: first_reply.map(|m| m.at),
        occurred_on: src[0].at.date(),
        asker: src[0].sender_id.clone(),
        asker_role: src[0].sender_role,
        agents,
        first_responder: first_reply.map(|m| m.sender_id.clone()),
        summary: d.summary.clone(),
        // `asker_role` 取 `src[0]`，这里取末条 —— 同一个 `src`，同一条规则的两端。
        last_msg_role: Some(src[src.len() - 1].sender_role),
        // 和 `last_msg_role` 同一条路：确定性计算，不经模型。
        followup_wait_max_sec: Some(followup_wait_max_sec(&src)),
        // 展示列 —— 和 `source_msg_ids` 走同一个 `src`，等长同序是构造上的事实，
        // 不需要再校验一遍（跟 `occurred_on` 同一条理由，见本函数头）。
        source_messages: src
            .iter()
            .map(|m| SourceMessage {
                msg_id: m.msg_id.clone(),
                at: m.at.format("%Y-%m-%d %H:%M:%S").to_string(),
                sender_id: m.sender_id.clone(),
                sender_role: m.sender_role.as_str(),
                // 非文本消息只留占位符：媒体 URL 带签名会过期，OCR 文本按体量算
                // 跟正文一样贵。判定在 `Message::placeholder` 一处。
                text: m
                    .placeholder()
                    .map_or_else(|| m.text.clone(), str::to_owned),
            })
            .collect(),
    };

    if ev.first_msg_time > ev.last_msg_time {
        return Err(format!("时间倒挂: {} > {}", ev.first_msg_time, ev.last_msg_time).into());
    }
    let n = ev.summary.chars().count();
    if n > SUMMARY_MAX {
        return Err(format!("summary 超长: {n} 字").into());
    }
    if ev.first_agent_reply_time.is_none() {
        if ev.first_responder.is_some() || !ev.agents.is_empty() {
            return Err(format!(
                "无平台回复却有 responder/agents: {:?} / {:?}",
                ev.first_responder, ev.agents
            )
            .into());
        }
    } else if !ev
        .first_responder
        .as_ref()
        .is_some_and(|r| ev.agents.contains(r))
    {
        return Err(format!(
            "first_responder {:?} 不在 agents {:?} 里",
            ev.first_responder, ev.agents
        )
        .into());
    }
    Ok(ev)
}

/// 模型分组质量的哨兵。**只打不改**，和 `llm.rs` 那条 `推理没关掉` 同一个位置。
///
/// `super::prompt::SYSTEM` 自己规定平台发起只有三种形态（结构化工单推送 /「三方：<单号>」/
/// 「<单号> 催促了」），**三种都带订单号**。所以「`asker_role=INTERNAL` 且全部来源消息
/// 里一个订单号都没有」不是平台发起的事件，是从商家事件尾巴上被撕下来的「已处理」
/// 「稍等」—— 由模型的分组失误制造出来的假事件。
///
/// 同一份 3742 条样本、同一份 prompt 实测：`glm-5.3-flash` 1 个，`qwen3.8-flash` 27 个。
/// 代价不在事件总数上，在指标上：撕下尾巴 = 商家那半边丢掉首响，p90 被系统性压低。
///
/// **不并回去是有意的**：这批消息上 `replyTo` 全是空的（`align` 唯一认的钥匙），
/// 剩下的候选钥匙都是猜 —— 过度合并比留着这几十个孤儿更糟。这里只负责让它**不静默**。
/// 换模型时它是第一个会喊的地方。
pub(super) fn orphans(drafts: &BTreeMap<u32, Draft>, msgs: &[Message]) -> usize {
    let n = drafts
        .values()
        .filter(|d| {
            msgs[d.idx[0]].sender_role == Role::Internal
                && !d.idx.iter().any(|&i| ORDER_NO.is_match(&body(&msgs[i])))
        })
        .count();
    if n > 0 {
        tracing::warn!(
            orphans = n,
            events = drafts.len(),
            "[孤儿] 平台发起事件不带订单号 —— 多半是被撕下来的应答尾巴，模型分组质量的哨兵"
        );
    }
    n
}

#[cfg(test)]
mod tests {
    use super::super::tests::{draft, msgs};
    use super::*;

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

        merge(
            &mut md,
            vec![mk(Some(1), vec![2], "已取消完毕", false)],
            200,
        );
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
        assert_eq!(
            got.keys().copied().collect::<Vec<_>>(),
            [1, 9],
            "该并的没并 / 保留的 ref 不对"
        );
        assert_eq!(got[&1].idx, [100, 101, 102], "idx 没取并集");
        assert_eq!(got[&1].summary, "早", "该保留 idx[0] 最小那个的 summary");
        assert_eq!(got[&9].idx, [500], "不相干的 draft 被动了");

        // 传递性：#500 再回复 #101，三个 draft 应并成一个
        linked[500].reply_to = Some(ms[101].msg_id.clone());
        let chain = align(probe, &linked);
        assert_eq!(
            chain.keys().copied().collect::<Vec<_>>(),
            [1],
            "传递闭包没闭合"
        );
        assert_eq!(chain[&1].idx, [100, 101, 102, 500]);
    }

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
        assert_eq!(
            ev.first_responder.as_deref(),
            Some(ms[2].sender_id.as_str())
        );
        assert_eq!(ev.occurred_on, ms[1].at.date());
        assert!(
            ev.agents.contains(&ms[2].sender_id),
            "首响人必须在 agents 里"
        );
    }

    /// 展示列：与 `source_msg_ids` 等长同序，非文本消息只留占位符。
    ///
    /// 错法都是静默的 —— 错位了溯源看起来还对，媒体 URL 漏进去要到账单上才发现。
    #[test]
    fn assemble_snapshots_the_source_messages_and_masks_non_text_ones() {
        let mut ms = msgs(10);
        ms[2].msg_type = Some("IMAGE".into());
        ms[2].text = "https://oss.example.com/signed?expires=1&sig=deadbeef".into();
        ms[4].msg_type = None; // 上游没给类型 —— 不猜，正文照常留
        ms[4].text = "没有类型的一条".into();
        let ev = assemble(&draft(&[1, 2, 4], "商家发了张图", false), &ms).unwrap();

        // 等长同序：错位了 source_msg_ids 和正文就对不上，而两边各自看都正常
        assert_eq!(
            ev.source_messages
                .iter()
                .map(|m| m.msg_id.as_str())
                .collect::<Vec<_>>(),
            ev.source_msg_ids,
        );
        assert_eq!(ev.source_messages[0].text, ms[1].text);
        assert_eq!(ev.source_messages[0].sender_role, "EXTERNAL");
        assert_eq!(
            ev.source_messages[0].at,
            ms[1].at.format("%Y-%m-%d %H:%M:%S").to_string()
        );
        // 图片：只留占位符，带签名的 URL 一个字节都不进库
        assert_eq!(ev.source_messages[1].text, "[图片]");
        assert!(!ev.source_messages[1].text.contains("oss.example.com"));
        // 没类型的按文本放行（白名单式判定，宁可多存也不静默丢正文）
        assert_eq!(ev.source_messages[2].text, "没有类型的一条");
    }

    #[test]
    fn assemble_leaves_first_response_null_when_no_platform_replied() {
        let ms = msgs(10);
        let ev = assemble(&draft(&[1, 3], "商家提了个要求，没人回", true), &ms).unwrap();
        assert!(ev.first_agent_reply_time.is_none() && ev.first_responder.is_none());
        assert!(ev.agents.is_empty(), "没有平台回复就不该有 agents");
    }

    /// `last_msg_role` 取末条，`asker_role` 取首条 —— **两端各取各的**。
    /// 错法是静默的：写成 `asker_role` 或首个 `INTERNAL` 都编译通过，
    /// 而「谁讲了最后一句」正好在多数事件上跟发起方相反。
    #[test]
    fn assemble_takes_the_tail_role_from_the_last_source_message() {
        let ms = msgs(10);
        // msgs 交替：奇数下标 EXTERNAL、偶数下标 INTERNAL。
        let ev = assemble(&draft(&[1, 2], "商家提要求，客服收的尾", false), &ms).unwrap();
        assert_eq!(ev.asker_role, Role::External);
        assert_eq!(ev.last_msg_role, Some(Role::Internal));
        // 反过来：平台发起，商家说完没人接。
        let ev = assemble(&draft(&[2, 3], "平台推工单，商家追问没人接", true), &ms).unwrap();
        assert_eq!(ev.asker_role, Role::Internal);
        assert_eq!(ev.last_msg_role, Some(Role::External));
    }

    /// 后续等待：**首响那一次不算、末尾没人接不算、非工作时段扣掉。**
    ///
    /// 三条错法全是静默的 —— 把首响重算一遍会让它和 `first_agent_reply_time`
    /// 打架；算上没终点的尾巴等于把 `last_msg_role` 的活重做一遍且随跑批时刻漂移；
    /// 不扣时段会把「晚上问、早上答」记成 12 小时失职。
    #[test]
    fn assemble_measures_followup_waits_in_working_hours_only() {
        // msgs 交替：偶数下标 INTERNAL、奇数下标 EXTERNAL，间隔 30 秒，从 09:00 起。
        let ms = msgs(6);
        // 只有首响一轮 —— 那是 first_agent_reply_time 的活，这里必须是 0 而不是 30。
        let ev = assemble(&draft(&[1, 2], "一问一答", false), &ms).unwrap();
        assert_eq!(ev.followup_wait_max_sec, Some(0));
        // 末尾没人接：那一段没有终点，不计。
        let ev = assemble(&draft(&[1, 2, 3], "答完又追问，没人接", true), &ms).unwrap();
        assert_eq!(ev.followup_wait_max_sec, Some(0));
        // 第二轮：09:01:30 追问 → 09:02:00 接话。
        let ev = assemble(&draft(&[1, 2, 3, 4], "追问后又接上了", false), &ms).unwrap();
        assert_eq!(ev.followup_wait_max_sec, Some(30));
        // 平台发起（首条就是 INTERNAL）：它的第一次 EXTERNAL→INTERNAL 是**真实的**
        // 后续等待，照算不跳 —— 首响在它身上恒等于首条，本来就没占掉哪一轮。
        let ev = assemble(&draft(&[2, 3, 4], "平台推工单，商家追问后接上", false), &ms).unwrap();
        assert_eq!(ev.first_agent_reply_time, Some(ms[2].at));
        assert_eq!(ev.followup_wait_max_sec, Some(30));

        // 20:50 追问 → 次日 09:00 接话：墙钟 12 小时 10 分，工作时段只有 10 分 + 30 分。
        let mut ms = msgs(6);
        let d = ms[0].at.date();
        ms[3].at = d.and_hms_opt(20, 50, 0).unwrap();
        ms[4].at = d.succ_opt().unwrap().and_hms_opt(9, 0, 0).unwrap();
        let ev = assemble(&draft(&[1, 2, 3, 4], "隔夜才接上", false), &ms).unwrap();
        assert_eq!(ev.followup_wait_max_sec, Some(10 * 60 + 30 * 60));
        // 整段落在时段外：22:00 追问、22:30 接话 —— 工作秒数是 0，不是 1800。
        ms[3].at = d.and_hms_opt(22, 0, 0).unwrap();
        ms[4].at = d.and_hms_opt(22, 30, 0).unwrap();
        let ev = assemble(&draft(&[1, 2, 3, 4], "深夜一来一回", false), &ms).unwrap();
        assert_eq!(ev.followup_wait_max_sec, Some(0));
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

    #[test]
    fn orphans_counts_platform_started_events_without_an_order_number() {
        let mut ms = msgs(10);
        ms[0].sender_role = Role::Internal;
        ms[2].sender_role = Role::Internal;
        ms[2].text = "工单原因：电话核实 订单号:JDLY202608031734008496".into();
        let drafts: BTreeMap<u32, Draft> = [
            (1u32, draft(&[0], "被撕下来的应答尾巴", false)), // INTERNAL 起头、无单号 -> 孤儿
            (2u32, draft(&[2], "平台推的工单", false)),       // INTERNAL 起头、有单号 -> 不是
            (3u32, draft(&[1], "商家发起", false)),           // EXTERNAL 起头 -> 不是
        ]
        .into();
        assert_eq!(orphans(&drafts, &ms), 1);
    }
}
