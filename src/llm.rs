//! LLM 调用层：一个连接池 + 结构化抽取。
//!
//! 四条实测结论决定了这里的写法（2026-09-01，dashscope 兼容端点 + qwen3.8-flash）：
//!
//! 1. **端点协商到 HTTP/2**。reqwest 走 ALPN 默认就拿到了，无需任何配置。
//!    不要设 `http2_prior_knowledge()` —— 那是给明文 h2c 用的，https 上设了会跳过
//!    协商，反而可能连不上。
//!
//! 2. **连接复用**：`Llm` 全进程只建一次，并发任务 clone 它 —— 内部 `reqwest::Client`
//!    是 Arc，clone 共享同一个连接池和 h2 多路复用连接。
//!    ⚠️ 但别把它当性能旋钮：握手成本实测约 85ms（curl 打 /models：首次 connect 6.7ms
//!    /total 143ms，复用后 0ms/58ms），而真实抽取调用光模型生成就波动 0.8~2.3s ——
//!    A/B 各跑 3 次「复用」对「每次新建」，**差异完全淹没在噪声里，测不出来**。
//!    复用的真正意义是并发跑几十个群时不去建几十条冗余连接、不给端点堆握手，
//!    不是让单次调用变快。想提速得去看 reasoning 和输出量，不是这里。
//!
//! 3. **传输层重试不用自己写**。async-openai 的 ReqwestExecutor 无条件挂了
//!    `OpenAIRetryLayer::default()`（executor.rs:147）：429/5xx/**连接错误**指数退避
//!    （100ms 起翻倍、封顶 8s），尊重 Retry-After，429 还会读 body 区分「限流」(重试)
//!    和「配额耗尽」(直接失败)。默认额外重试 3 次。
//!
//! 4. **超时不被那一层重试** —— 读过源码确认，不是推测：`retry/openai.rs:239` 的
//!    `is_connection_error` 只认 `reqwest::Error::is_connect()`，超时走
//!    `Err(error) => return Err(error)` 当场返回。这一条是承重的：超时是 ADR-0004
//!    二分的触发信号之一，若被重试 3 次吃掉，`timeout_secs=300` 会变成最坏 20 分钟
//!    才浮出一个本该立刻切分的信号。**换 async-openai 版本时要重新确认这一条。**

use crate::{BoxError, config::LlmConfig};
use async_openai::{
    Client,
    config::OpenAIConfig,
    error::OpenAIError,
    types::chat::{
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, FinishReason, ReasoningEffort, ResponseFormat,
        ResponseFormatJsonSchema,
    },
};
use schemars::{JsonSchema, schema_for};
use serde::de::DeserializeOwned;
use std::{fmt, time::Duration};

/// TCP keepalive 间隔。长连接空闲久了会被中间设备静默掐断，下次复用就是一个莫名其妙的
/// connection reset。跑批是连续调用，这个数只在群与群的空档里起作用。
/// 具名而不内联，跟 `pull.rs` 的 [`crate::pull`] 那几个超时常量一个规矩。
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

/// 对话里的一轮。**存在的唯一理由是校验失败要重问一次**（③ 的 `MAX_RETRIES = 1`）：
/// 把模型上一次的原始输出放回 `Assistant`、把校验报错放进新的 `User`，它才知道要改什么。
/// 只发一条 `User` 的话，模型看不见自己错在哪。
#[derive(Debug)]
pub enum Turn {
    User(String),
    Assistant(String),
}

/// 调用失败的三类，**处置方式不同所以必须在类型上分开**（跟 `IngestError` 一个规矩）。
///
/// 前两个是 ADR-0004 说的「模型吃不下这一段」的两种表现，要翻译成同一个切分信号；
/// `Other` 一律不切。这条分界错一边的代价不对称：把连接错误当成「太大」，
/// 会把一次网络故障放大成一整棵二分调用树（切成两半，两半照样断，再各切两半……）。
#[derive(Debug)]
pub enum LlmError {
    /// `finish_reason == Length` —— 输出预算耗尽，被截断了。
    ///
    /// **必须在解析 JSON 之前判掉。** 截断的 JSON 解析必然失败，而那会把
    /// 「预算耗尽」伪装成一次语法错误 —— `CLAUDE.md` 硬规则点名禁止的形态。
    Truncated,
    /// 连上了，但这一次没跑完（`reqwest` 超时）。
    Timeout,
    /// 其余全部。**连接错误在这里**，不在上面两个里。
    Other(BoxError),
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("输出被截断（finish_reason=length），输出预算耗尽"),
            Self::Timeout => f.write_str("请求超时"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LlmError {}

/// 超时单独分出来，其余都是 `Other` —— **连接错误也是 `Other`**。
impl From<OpenAIError> for LlmError {
    fn from(e: OpenAIError) -> Self {
        match &e {
            OpenAIError::Reqwest(r) if r.is_timeout() => Self::Timeout,
            _ => Self::Other(Box::new(e)),
        }
    }
}

/// 抽取结果连带这次调用的用量。
///
/// 用量不是可选的装饰：ROOM_CONCURRENCY 的上限是端点 TPM 算出来的，而现有估算是拿
/// 字符数折的、偏差未知。上层把这里的数字累加起来，才能把那个估算坐实。
#[derive(Debug)]
pub struct Extracted<T> {
    pub data: T,
    /// 模型返回的原始 JSON。校验不过时要把它原样放进 [`Turn::Assistant`] 再问一次。
    pub raw: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// 推理 token。抽取任务这里应该恒为 0 —— 不为 0 就说明 reasoning_effort 配错了，
    /// 在白烧钱。别信"没报错"，就看这个数。
    pub reasoning_tokens: u32,
}

/// 全进程共享一个。clone 是廉价的，连接池跟着一起共享。
#[derive(Clone)]
pub struct Llm {
    client: Client<OpenAIConfig>,
    model: String,
    reasoning_effort: ReasoningEffort,
    temperature: f32,
    max_tokens: u32,
}

impl Llm {
    pub fn new(cfg: &LlmConfig, api_key: String) -> crate::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
            .tcp_keepalive(TCP_KEEPALIVE)
            .build()?;

        Ok(Self {
            client: Client::with_config(
                OpenAIConfig::new()
                    .with_api_key(api_key)
                    .with_api_base(&cfg.base_url),
            )
            .with_http_client(http),
            model: cfg.model.clone(),
            reasoning_effort: cfg.reasoning_effort.clone(),
            temperature: cfg.temperature,
            max_tokens: cfg.max_tokens,
        })
    }

    /// 按 T 的 schema 抽一个结构化结果出来。
    ///
    /// `system` 是角色指令，`turns` 是对话本体 —— 正常一轮就一条 [`Turn::User`]，
    /// 校验失败重问时是三条（原问 / 模型的错误输出 / 报错）。
    pub async fn extract<T>(
        &self,
        system: &str,
        turns: &[Turn],
    ) -> std::result::Result<Extracted<T>, LlmError>
    where
        T: JsonSchema + DeserializeOwned,
    {
        let (name, schema) = strict_schema::<T>();

        let mut messages: Vec<ChatCompletionRequestMessage> =
            vec![ChatCompletionRequestSystemMessageArgs::default()
                .content(system)
                .build()
                .map_err(LlmError::from)?
                .into()];
        for t in turns {
            messages.push(match t {
                Turn::User(s) => ChatCompletionRequestUserMessageArgs::default()
                    .content(s.as_str())
                    .build()
                    .map_err(LlmError::from)?
                    .into(),
                Turn::Assistant(s) => ChatCompletionRequestAssistantMessageArgs::default()
                    .content(s.as_str())
                    .build()
                    .map_err(LlmError::from)?
                    .into(),
            });
        }

        let request = CreateChatCompletionRequestArgs::default()
            .model(&self.model)
            .reasoning_effort(self.reasoning_effort.clone())
            .temperature(self.temperature)
            .max_tokens(self.max_tokens)
            .response_format(ResponseFormat::JsonSchema {
                json_schema: ResponseFormatJsonSchema {
                    name,
                    description: None,
                    schema,
                    strict: Some(true),
                },
            })
            .messages(messages)
            .build()
            .map_err(LlmError::from)?;

        // async-openai 全仓只埋了 6 处 tracing 点，且全是 warn 级别：429 的 rate-limit
        // 和 retry-after header、5xx 服务端错误。默认 info 就能看到它们（warn 更高）。
        // 它没有任何 debug/trace 事件 —— 把级别调详细也看不到请求体，要抓请求得在这里自己加。
        let started = std::time::Instant::now();
        let response = self.client.chat().create(request).await?;
        let elapsed = started.elapsed();

        let choice = response.choices.first().ok_or_else(|| {
            LlmError::Other("模型没返回任何 choice".into())
        })?;
        // **先判截断，再解析。** 顺序反了，截断就会表现成一次 JSON 语法错误。
        if choice.finish_reason == Some(FinishReason::Length) {
            return Err(LlmError::Truncated);
        }
        let content = choice
            .message
            .content
            .as_deref()
            .ok_or_else(|| LlmError::Other("模型没返回内容".into()))?;

        let usage = response.usage.unwrap_or_default();
        let reasoning_tokens = usage
            .completion_tokens_details
            .and_then(|d| d.reasoning_tokens)
            .unwrap_or(0);

        if reasoning_tokens > 0 {
            // 抽取任务不该有推理。到这里说明 reasoning_effort 配错了，在白烧钱。
            tracing::warn!(
                reasoning_tokens,
                "推理没关掉，检查 config.toml 的 reasoning_effort"
            );
        }
        tracing::info!(
            elapsed_ms = elapsed.as_millis(),
            prompt_tokens = usage.prompt_tokens,
            completion_tokens = usage.completion_tokens,
            reasoning_tokens,
            "抽取完成"
        );

        Ok(Extracted {
            data: serde_json::from_str(content).map_err(|e| {
                LlmError::Other(format!("模型输出不是合法的 {}: {e}", std::any::type_name::<T>()).into())
            })?,
            raw: content.to_string(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            reasoning_tokens,
        })
    }
}

/// 把 schemars 的 schema 改成能用的形状，返回 (schema 名, schema)。
///
/// schemars 生成的不能直接用：Option 字段不进 required，模型就把整个键跳过不输出
/// （实测 owner 字段直接从返回里消失）。补全 required 让"可选"变成"输出 null"。
/// additionalProperties 和去掉 $schema 是 OpenAI strict 的要求 —— dashscope 其实
/// 不检查这两条，但对着规范写，换回 OpenAI 时不用再改。
///
/// ⚠️ 只补顶层的 required。嵌套类型（`$defs` 里的）走 schemars 自己那套 ——
/// ③ 的 `SegmentExtraction` 顶层只有一个必填的 `events` 数组，元素类型另行处理。
fn strict_schema<T: JsonSchema>() -> (String, serde_json::Value) {
    let mut schema = serde_json::to_value(schema_for!(T)).expect("schema 转 json 失败");
    let obj = schema.as_object_mut().expect("schema 顶层不是 object");
    obj.remove("$schema");

    // schemars 把类型名放在 title 里，正好当 response_format 的 name 用
    let name = obj
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("output")
        .to_string();

    let fields: Vec<serde_json::Value> = obj["properties"]
        .as_object()
        .expect("schema 缺 properties")
        .keys()
        .map(|k| k.as_str().into())
        .collect();
    obj.insert("required".into(), fields.into());
    obj.insert("additionalProperties".into(), false.into());
    (name, schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(JsonSchema, Deserialize)]
    #[allow(dead_code)]
    struct Sample {
        required_field: String,
        optional_field: Option<String>,
    }

    // Option 字段也要进 required，否则模型会跳过不输出
    #[test]
    fn optional_fields_must_be_required() {
        let (name, schema) = strict_schema::<Sample>();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        assert_eq!(name, "Sample");
        assert!(required.contains(&"optional_field"), "Option 字段漏了就会丢数据");
        assert_eq!(required.len(), 2);
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        assert!(schema.get("$schema").is_none());
    }
}
