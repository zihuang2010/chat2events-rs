//! 唯一一条真打端点的路径。跑一次就能确认：配置、鉴权、**嵌套结构化输出**
//! （`SegmentExtraction` -> `$defs/EventDraft`，dashscope 认不认 `$ref` + strict）、
//! 以及 `reasoning_effort` 有没有关掉（看输出里那个 `推理 0`，别信「没报错」）。
//!
//! ```sh
//! cargo run --example smoke -- .
//! ```
//!
//! 接的是**生产的 `LiveModel`**，不是另开一条路径 —— 否则验的就不是要跑的东西。
//! 不连 MySQL、不写任何表。
use chat2events_rs::{
    config, extract,
    ingest::{Message, Role},
    llm::Llm,
};
use chrono::NaiveDate;
use std::collections::BTreeSet;

fn msg(i: usize, at: (u32, u32, u32), role: Role, who: &str, text: &str) -> Message {
    Message {
        msg_id: format!("m{i}"),
        room: "R".into(),
        corp: "C".into(),
        at: NaiveDate::from_ymd_opt(2026, 8, 29)
            .unwrap()
            .and_hms_opt(at.0, at.1, at.2)
            .unwrap(),
        sender_id: who.into(),
        sender_role: role,
        text: text.into(),
        reply_to: None,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let (cfg, secrets) = config::load_from_dir(std::path::Path::new(&dir));
    let llm = Llm::new(&cfg.llm, secrets.llm.api_key)?;
    let model = extract::LiveModel::new(llm);

    let msgs = vec![
        msg(1, (10, 2, 0), Role::External, "u1", "5127366458053009229  加14个筒灯"),
        msg(2, (10, 2, 5), Role::External, "u1", "[图片消息]"),
        msg(3, (10, 3, 0), Role::Internal, "u2", "稍等"),
        msg(4, (10, 11, 0), Role::Internal, "u2", "已加单"),
        msg(5, (10, 12, 0), Role::External, "u1", "好的谢谢"),
        msg(6, (14, 30, 0), Role::External, "u3", "3316977912130066680 客户要求改期到周六，客户电话13581496310"),
    ];

    // 走生产的 view -> LiveModel::call（含校验与重灌），只是把 drafts 传空
    let events = extract::extract(&msgs, &model, 400).await?;
    println!("抽出 {} 个事件：", events.len());
    for e in &events {
        println!(
            "  · [{}] {} | 来源 {:?} | 首响 {:?} | asker_role={}",
            e.occurred_on, e.summary, e.source_msg_ids, e.first_agent_reply_time,
            e.asker_role.as_str()
        );
    }
    // 承重不变量 6：溯源 ID 必须真实存在于这次抽取的消息里
    let known: BTreeSet<&str> = msgs.iter().map(|m| m.msg_id.as_str()).collect();
    for e in &events {
        assert!(!e.source_msg_ids.is_empty(), "溯源不能为空");
        assert!(
            e.source_msg_ids.iter().all(|id| known.contains(id.as_str())),
            "模型编造了 msg_id —— 序号映射坏了"
        );
    }
    println!("\n溯源校验通过（{} 个事件的来源 ID 全部真实存在）", events.len());
    Ok(())
}
