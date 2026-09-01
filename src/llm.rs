//! LLM 调用层：一个连接池 + 结构化抽取。
//!
//! 三条实测结论决定了这里的写法（2026-09-01，dashscope 兼容端点 + qwen3.8-flash）：
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
//!    `OpenAIRetryLayer::default()`（executor.rs:147）：429/5xx/连接错误指数退避，
//!    尊重 Retry-After，429 还会读 body 区分「限流」(重试) 和「配额耗尽」(直接失败)。
//!    默认额外重试 3 次。⚠️ 副作用见 `LlmConfig::timeout_secs` 的注释。

use crate::{Result, config::LlmConfig};
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequestArgs, ReasoningEffort,
        ResponseFormat, ResponseFormatJsonSchema,
    },
};
use schemars::{JsonSchema, schema_for};
use serde::de::DeserializeOwned;
use std::time::Duration;

/// 抽取结果连带这次调用的用量。
///
/// 用量不是可选的装饰：ROOM_CONCURRENCY 的上限是端点 TPM 算出来的，而现有估算是拿
/// 字符数折的、偏差未知。上层把这里的数字累加起来，才能把那个估算坐实。
#[derive(Debug)]
pub struct Extracted<T> {
    pub data: T,
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
    pub fn new(cfg: &LlmConfig, api_key: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
            // 长连接空闲久了会被中间设备静默掐断，下次复用就是一个莫名其妙的 connection
            // reset。keepalive 让它别断。跑批是连续调用，池子的默认空闲时间够用。
            .tcp_keepalive(Duration::from_secs(60))
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
    pub async fn extract<T>(&self, prompt: &str) -> Result<Extracted<T>>
    where
        T: JsonSchema + DeserializeOwned,
    {
        let (name, schema) = strict_schema::<T>();

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
            .messages([ChatCompletionRequestUserMessageArgs::default()
                .content(prompt)
                .build()?
                .into()])
            .build()?;

        // async-openai 全仓只埋了 6 处 tracing 点，且全是 warn 级别：429 的 rate-limit
        // 和 retry-after header、5xx 服务端错误。默认 info 就能看到它们（warn 更高）。
        // 它没有任何 debug/trace 事件 —— 把级别调详细也看不到请求体，要抓请求得在这里自己加。
        let started = std::time::Instant::now();
        let response = self.client.chat().create(request).await?;
        let elapsed = started.elapsed();
        let content = response
            .choices
            .first()
            .and_then(|c| c.message.content.as_deref())
            .ok_or("模型没返回内容")?;

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
            data: serde_json::from_str(content)?,
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
