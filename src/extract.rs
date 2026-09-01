//! ③ 抽取 extract —— 把会话变成结构化事件。
//!
//! ⚠️ **目前只有一段冒烟，真正的 ③ 还没搬过来。** 缺的是：自适应二分 · 段间便签 ·
//! 序号↔`msg_id` 映射 · 溯源校验 · 正文脱敏（`_body`）。
//! 见 Python 版的 `src/extract.py` 与 ADR-0001 / 0002 / 0004。
//!
//! 冒烟留着是有用的：它是唯一一条真打端点的路径，跑一次就能确认
//! 配置、鉴权、结构化输出、以及 `reasoning_effort` 有没有关掉。

use crate::{Result, llm::Llm};
use schemars::JsonSchema;
use serde::Deserialize;

/// ⚠️ **这不是 `CONTEXT.md` 里那个 `Event`。** 真正的 `Event` 有 11 个字段由 ④ 装配
/// 从真实消息算出（`source_msg_ids` / `occurred_on` / `first_agent_reply_time` …），
/// 一个都不采信模型。这里只是探路用的形状。
#[derive(JsonSchema, Deserialize, Debug)]
#[schemars(description = "售后派单群里的一次报修事件")]
#[allow(dead_code)] // 字段目前只经 Debug 输出，落库那步会真正读到
struct RepairEvent {
    /// 订单号，没提到就填 null
    order_no: Option<String>,
    /// 报修的问题，一句话
    issue: String,
    /// 谁负责跟进，用群里出现的名字
    owner: Option<String>,
    /// 当前状态
    status: String,
}

/// 硬编码样本 —— 冒烟用，**不是**上面 ①② 读出来的会话。
const CHAT: &str = "\
[10:02] 商家客服-王姐：订单 SO20260812001 客户反馈客厅吊灯不亮，通电没反应
[10:03] 平台客服-小陈：稍等，我看下
[10:11] 平台客服-小陈：已派给李工，今天下午上门
[10:12] 商家客服-王姐：好的谢谢";

/// 打一次真实端点，把结果和用量印到 stdout。
///
/// 结果走 stdout（与日志分流的理由见 `main.rs::init_logging`）。
pub async fn smoke(llm: &Llm) -> Result<()> {
    let result = llm
        .extract::<RepairEvent>(&format!("从下面这段群聊里抽出报修事件：\n\n{CHAT}"))
        .await?;

    println!("{:#?}", result.data);
    println!(
        "\n输入 {} token，输出 {} token，其中推理 {}",
        result.prompt_tokens, result.completion_tokens, result.reasoning_tokens
    );
    // 推理关没关就看上面那个 0。别信"没报错"。
    Ok(())
}
