//! LLM 调用层：一个连接池 + 结构化抽取。
//!
//! 四条实测结论决定了这里的写法（2026-09-01，dashscope 兼容端点 + qwen3.8-flash）：
//!
//! 1. **全程 HTTP/1.1，没有 h2 —— 有意的**。`Cargo.toml` 给 reqwest 关了默认 feature
//!    且没开 `http2`（`Cargo.lock` 里根本没有 `h2` 这个 crate），所以 ALPN 不会去协商
//!    h2。⚠️ 早先这里写着「端点协商到 HTTP/2，走 ALPN 默认就拿到了」—— 那是拿 curl
//!    量的，**不是这个二进制的行为**，别照着它推理。
//!    不开的理由：负载是 8 路并发的长请求（单次实测 118~168s），一路一条 1.1 连接就够，
//!    多路复用在这里买不到东西，不值得多背一个 `h2` 依赖。
//!
//! 2. **连接复用**：`Llm` 全进程只建一次，并发任务 clone 它 —— 内部 `reqwest::Client`
//!    是 Arc，clone 共享同一个 HTTP/1.1 连接池。
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
//! 4. **连接后的响应超时不被那一层重试**：`retry/openai.rs:239` 的
//!    `is_connection_error` 只认 `reqwest::Error::is_connect()`，响应超时走
//!    `Err(error) => return Err(error)` 当场返回。这一条是承重的：超时是自适应
//!    二分的触发信号之一，若被重试 3 次吃掉，`timeout_secs=300` 会变成最坏 20 分钟
//!    才浮出一个本该立刻切分的信号。**换 async-openai 版本时要重新确认这一条。**
//!    握手超时同时属于 connect 与 timeout，仍按连接故障重试，不能触发切段。
//!    完整逻辑调用另有总预算，SDK 的 Retry-After 等待也包含在内。

use crate::{
    BoxError,
    config::{LlmModel, LlmSection},
};
use async_openai::{
    Client,
    config::{Config as _, OpenAIConfig},
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
/// 具名而不内联，跟 `mirror/download.rs` 那几个超时常量一个规矩。
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

/// [`Llm::extract_retry`] 跑飞重发几次。跑飞是随机的、与输入规模无关，单次概率
/// 实测约三分之一（2026-09-02，十余次调用）：重发 4 次后仍全中约 0.4%。
/// 曾经是 2（单点 3.7%）—— 归纳 + 试打是几十次串行调用的长流程，单点 3.7% 摊到
/// 八九次调用上就是约四分之一的概率掀翻整趟，正是「特别不稳」的主因之一。
/// 每次代价 = 生成满输出上限的时间（4000 token 约半分钟），封顶可承受。
const RUNAWAY_RETRIES: u32 = 4;

/// [`Llm::extract_retry`] 超时重发几次。超时一次要吃满 `timeout_secs`（分钟级），
/// 预算给小：偶发的端点抽风一次重发就能过，连着两次超时更可能是端点真出事了，
/// 该报出来让人看，不该再安静地挂几分钟。
const TIMEOUT_RETRIES: u32 = 1;

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
/// 前两个是「模型吃不下这一段」的两种表现，要翻译成同一个切分信号；
/// `Other` 一律不切。这条分界错一边的代价不对称：把连接错误当成「太大」，
/// 会把一次网络故障变成无效的逐级切分，直到最小子段仍失败。
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
            // 握手失败也可能 is_timeout；只有已经连上后的超时才交给抽取切段。
            OpenAIError::Reqwest(r) if r.is_timeout() && !r.is_connect() => Self::Timeout,
            _ => Self::Other(Box::new(e)),
        }
    }
}

/// 一次调用的结果 —— **只有两样，因为调用方只读这两样**。
///
/// ⚠️ **用量不在这里**：三个 token 数曾经是 pub 字段，而全仓唯一的读取点是
/// [`Llm::extract`] 自己那两行日志（`推理没关掉` 的告警 + 一行 info）。
/// ROOM_CONCURRENCY 的上限确实要靠端点 TPM 坐实，但那需要的是**跨调用累加**，
/// 不是每次调用带回一份没人接的数字 —— 真做那件事时再让它们出面。
#[derive(Debug)]
pub struct Extracted<T> {
    pub data: T,
    /// 模型返回的原始 JSON。校验不过时要把它原样放进 [`Turn::Assistant`] 再问一次。
    pub raw: String,
}

/// 全进程共享一个。clone 是廉价的，连接池跟着一起共享。
#[derive(Clone)]
pub struct Llm {
    client: Client<OpenAIConfig>,
    model: String,
    reasoning_effort: ReasoningEffort,
    temperature: f32,
    max_tokens: u32,
    request_timeout: Duration,
}

impl Llm {
    /// 分类缓存使用实际请求配置的身份，不包含凭证或仅影响等待时间的配置。
    pub(crate) fn cache_identity(&self) -> crate::Result<String> {
        Ok(serde_json::to_string(&(
            self.client.config().api_base(),
            &self.model,
            &self.reasoning_effort,
            self.temperature,
            self.max_tokens,
        ))?)
    }

    pub(crate) fn model_name(&self) -> &str {
        &self.model
    }

    /// `cfg` 是两队共用的那几个键，`m` 是这一队自己的模型 / 端点 / 输出上限。
    ///
    /// **调用方必须显式选边**（`&cfg.llm.extract` 或 `&cfg.llm.classify`）——
    /// 拆成两个参数就是为了让「这一处是抽取还是打标」在调用点上写出来，
    /// 而不是靠上下文猜。
    pub fn new(cfg: &LlmSection, m: &LlmModel, api_key: String) -> crate::Result<Self> {
        if cfg.timeout_secs == 0 || cfg.connect_timeout_secs == 0 || cfg.request_timeout_secs == 0 {
            return Err("模型连接、请求与总预算必须大于零".into());
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
            .tcp_keepalive(TCP_KEEPALIVE)
            .build()?;

        Ok(Self {
            client: Client::with_config(
                OpenAIConfig::new()
                    .with_api_key(api_key)
                    .with_api_base(&m.base_url),
            )
            .with_http_client(http),
            model: m.model.clone(),
            reasoning_effort: cfg.reasoning_effort.clone(),
            temperature: cfg.temperature,
            max_tokens: m.max_tokens,
            request_timeout: Duration::from_secs(cfg.request_timeout_secs),
        })
    }

    /// [`extract`](Self::extract) 外面包一层**坏运气重发**：跑飞（`Truncated`）和
    /// 超时各自有限次重发（[`RUNAWAY_RETRIES`] / [`TIMEOUT_RETRIES`]），其余错误原样返回。
    ///
    /// 给 ⑤ 打标和 taxonomy 归纳用。这两条线的输出上限都贴着实际需求给（几千 token，
    /// 正常输出只用几百），撞上 `Truncated` 只可能是模型陷入重复生成 ——
    /// qwen3.8-flash 在 strict JSON schema 下随机中招，与输入规模无关（2026-09-02
    /// 实测约三分之一）—— 重发就是正确处置，静默等死才是错的。
    ///
    /// ⚠️ **③ 抽取不许用这个。** 那边 `Truncated` / `Timeout` 是「这段太大、
    /// 对半切」的信号，包上重发会掩盖真正的切分需求。所以重发是独立方法，
    /// 不是 `extract` 的默认行为 —— ③ 继续裸调 `extract`。
    pub async fn extract_retry<T>(
        &self,
        system: &str,
        turns: &[Turn],
    ) -> std::result::Result<Extracted<T>, LlmError>
    where
        T: JsonSchema + DeserializeOwned,
    {
        let (mut runaways, mut timeouts) = (0u32, 0u32);
        let deadline = tokio::time::Instant::now() + self.request_timeout;
        loop {
            match self.extract_until(system, turns, deadline).await {
                // 静默重发等于不知道模型在跑飞。这两条 warn 是唯一的信号。
                Err(LlmError::Truncated) if runaways < RUNAWAY_RETRIES => {
                    runaways += 1;
                    tracing::warn!(
                        attempt = runaways,
                        "模型跑飞（输出撞 max_tokens 上限），重发"
                    );
                }
                Err(LlmError::Timeout) if timeouts < TIMEOUT_RETRIES => {
                    timeouts += 1;
                    tracing::warn!(attempt = timeouts, "请求超时，重发");
                }
                other => return other,
            }
        }
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
        self.extract_until(
            system,
            turns,
            tokio::time::Instant::now() + self.request_timeout,
        )
        .await
    }

    async fn extract_until<T>(
        &self,
        system: &str,
        turns: &[Turn],
        deadline: tokio::time::Instant,
    ) -> std::result::Result<Extracted<T>, LlmError>
    where
        T: JsonSchema + DeserializeOwned,
    {
        let (name, schema) = strict_schema::<T>();

        let mut messages: Vec<ChatCompletionRequestMessage> = vec![
            ChatCompletionRequestSystemMessageArgs::default()
                .content(system)
                .build()
                .map_err(LlmError::from)?
                .into(),
        ];
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
        // 总预算覆盖 SDK 内部的退避等待；到期不是“段太大”，不能重新切段或重发。
        let response = tokio::time::timeout_at(deadline, self.client.chat().create(request))
            .await
            .map_err(|_| LlmError::Other("模型调用总预算耗尽（包含重试与等待）".into()))??;
        let elapsed = started.elapsed();

        let choice = response
            .choices
            .first()
            .ok_or_else(|| LlmError::Other("模型没返回任何 choice".into()))?;
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
        // 抽取任务这里应该恒为 0 —— 不为 0 就说明 reasoning_effort 配错了，在白烧钱。
        // 别信「没报错」，就看这个数。
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
                LlmError::Other(
                    format!("模型输出不是合法的 {}: {e}", std::any::type_name::<T>()).into(),
                )
            })?,
            raw: content.to_string(),
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
/// 所有嵌套对象也执行相同约束，包含 `$defs` 中的 WireDraft 与 Assignment。
fn strict_schema<T: JsonSchema>() -> (String, serde_json::Value) {
    use schemars::transform::{RecursiveTransform, Transform};
    let mut schema = schema_for!(T);
    RecursiveTransform(|schema: &mut schemars::Schema| {
        if let Some(properties) = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
        {
            let fields: Vec<serde_json::Value> =
                properties.keys().map(|key| key.as_str().into()).collect();
            schema.insert("required".into(), fields.into());
            schema.insert("additionalProperties".into(), false.into());
        }
    })
    .transform(&mut schema);
    let mut schema = serde_json::to_value(schema).expect("schema 转 json 失败");
    let obj = schema.as_object_mut().expect("schema 顶层不是 object");
    obj.remove("$schema");

    // schemars 把类型名放在 title 里，正好当 response_format 的 name 用
    let name = obj
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("output")
        .to_string();

    (name, schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, JsonSchema, Deserialize)]
    #[allow(dead_code)]
    struct Sample {
        required_field: String,
        optional_field: Option<String>,
    }

    #[tokio::test]
    async fn tls_handshake_timeout_is_a_connection_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.unwrap();
            // 接受 TCP 但不完成 TLS，稳定复现同时属于 connect 与 timeout 的错误。
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(connection);
        });
        let error = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_millis(100))
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
            .get(format!("https://{address}"))
            .send()
            .await
            .unwrap_err();
        assert!(error.is_connect() && error.is_timeout());
        let mapped = LlmError::from(OpenAIError::Reqwest(error));
        server.abort();
        let _ = server.await;
        assert!(
            matches!(mapped, LlmError::Other(_)),
            "握手失败不应成为切段信号：{mapped}"
        );
    }

    #[tokio::test]
    async fn retry_after_cannot_outlive_the_logical_request_budget() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut header = Vec::new();
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                connection.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let header = String::from_utf8(header).unwrap().to_ascii_lowercase();
            let length: usize = header
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            connection.read_exact(&mut vec![0; length]).unwrap();
            let body = r#"{"error":{"message":"rate limit","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#;
            write!(connection, "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 86400\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let mut cfg: crate::config::Config = toml::from_str(
            &include_str!("../config.toml")
                .replace("request_timeout_secs = 1200", "request_timeout_secs = 1"),
        )
        .unwrap();
        cfg.llm.extract.base_url = base;
        let llm = Llm::new(&cfg.llm, &cfg.llm.extract, "test-only-key".into()).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            llm.extract_retry::<Sample>("test", &[Turn::User("test".into())]),
        )
        .await;
        server.join().unwrap();
        let error = result
            .expect("SDK 的 Retry-After 必须受总预算约束")
            .unwrap_err();
        assert!(
            matches!(error, LlmError::Other(_)),
            "总预算耗尽不能触发重发或切段"
        );
        assert!(error.to_string().contains("总预算"));
    }

    #[tokio::test]
    async fn retry_reissues_truncation_but_stops_at_its_budget() {
        use crate::testutil::{completion, http_model, test_llm};
        let (base, server) = http_model(
            vec![
                (200, completion("{broken", "length")),
                (
                    200,
                    completion(r#"{"required_field":"ok","optional_field":null}"#, "stop"),
                ),
            ],
            false,
        );
        test_llm(&base, "test")
            .extract_retry::<Sample>("test", &[Turn::User("test".into())])
            .await
            .unwrap();
        assert_eq!(server.join().unwrap().len(), 2);
        let replies = (0..=RUNAWAY_RETRIES)
            .map(|_| (200, completion("{broken", "length")))
            .collect();
        let (base, server) = http_model(replies, false);
        assert!(matches!(
            test_llm(&base, "test")
                .extract_retry::<Sample>("test", &[Turn::User("test".into())])
                .await,
            Err(LlmError::Truncated)
        ));
        assert_eq!(server.join().unwrap().len(), RUNAWAY_RETRIES as usize + 1);
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
        assert!(
            required.contains(&"optional_field"),
            "Option 字段漏了就会丢数据"
        );
        assert_eq!(required.len(), 2);
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        assert!(schema.get("$schema").is_none());
    }
}
