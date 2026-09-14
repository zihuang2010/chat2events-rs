//! `llm` 的测试 —— 拆到这里的理由见 `src/lib.rs` 的组织规则。

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
        &include_str!("../../config.toml")
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
