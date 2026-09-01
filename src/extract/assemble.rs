//! ④ 装配 —— 段输出 → 全群 draft → [`Event`]。
//!
//! **明确不给端口**：它是承重不变量 6（溯源）的守卫，给它接缝等于给溯源留绕过口。
//!
//! 四件事按调用顺序住在这里：
//!   * [`merge`] —— 段内行号 → 全局下标的换算，**唯一发生地**（承重不变量 6）；
//!   * [`align`] —— 显式 `replyTo` 跨 draft 就并（**ADR-0002**，钥匙只有 `replyTo`）；
//!   * [`assemble`] —— 除 `summary` 外每个字段都从真实消息算，承重校验就在那里；
//!   * [`orphans`] —— 模型分组质量的哨兵，**只打不改**。

use super::{
    Draft, Event, EventDraft, SUMMARY_MAX,
    redact::{ORDER_NO, body},
};
use crate::{
    BoxError,
    ingest::{Message, Role},
};
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
/// 订单号是「工单」的钥匙不是「事件」的钥匙（ADR-0002）。
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
        let Some(rt) = m.reply_to.as_deref() else { continue };
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
        out.insert(keep, Draft { idx, ..drafts[&keep].clone() });
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

/// 除 `summary` 外的每一个字段都在这里从真实消息算出。
///
/// 承重不变量住在这里，**不在调用方** —— 「模型输出必须先校验再落库」，而调用方会忘记调。
/// 只留承重的几条：`occurred_on` 的定义在本函数里构造上恒真，不重复校验。
///
/// ⚠️ **一律 `Result` 不用 `panic!`**（`CLAUDE.md` 的 Rust 规矩，和 Python 版理由不同）：
/// `assert!` 在 release 下不会消失，但它会掀掉整轮 —— 而承重不变量 3 要求「某个群失败
/// → 该群跳过一行不写，**整轮继续**」。所以群级失败一律走 `Result`。
pub(super) fn assemble(d: &Draft, msgs: &[Message]) -> Result<Event, BoxError> {
    if d.idx.is_empty() {
        return Err("source_msg_ids 必须非空（承重不变量 6：溯源）".into());
    }
    let src: Vec<&Message> = d.idx.iter().map(|&i| &msgs[i]).collect();
    let internal: Vec<&&Message> = src.iter().filter(|m| m.sender_role == Role::Internal).collect();
    let first_reply = internal.first().copied();

    // agents 是**插入序去重**（Python 的 dict.fromkeys），不是排序 —— 顺序即出场顺序。
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
/// [`SYSTEM`] 自己规定平台发起只有三种形态（结构化工单推送 /「三方：<单号>」/
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
