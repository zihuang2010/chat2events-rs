//! 只读工作台的**资源上限**与错误映射 —— 四道闸，全部「拒绝」而不是「截断」。
//!
//! 1. [`admit`]：并发名额 + 整请求超时。拿不到名额就 503，不排队。
//! 2. [`ReadBudget`]：边读边扣行数与字节，**不能读完大集合再发现超限**。
//! 3. [`ResponseBuffer`]：序列化时按字节封顶，超了就 413。
//! 4. [`WebError`]：所有失败在这里统一成状态码，日志留全文、响应只给一句话。
//!
//! 超限一律是显式错误 —— 截断会让主管拿到一个偏小但看起来正常的数字，
//! 跟承重不变量 4「绝不用 0 表示没算出来」同一个形状。

use super::serve::WebState;
use crate::{BoxError, config::WebLimits};
use axum::{
    Json,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;

pub(super) async fn admit(
    State(state): State<WebState>,
    request: Request,
    next: middleware::Next,
) -> Response {
    let Ok(_permit) = state.requests.try_acquire() else {
        return WebError(
            StatusCode::SERVICE_UNAVAILABLE,
            "查询繁忙，请稍后重试".into(),
        )
        .into_response();
    };
    // 名额覆盖取数与响应序列化；等待超时取消数据库读取，原文阻塞任务另持真实扫描名额。
    match tokio::time::timeout(
        Duration::from_secs(state.limits.query_timeout_secs),
        next.run(request),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => WebError(
            StatusCode::GATEWAY_TIMEOUT,
            "查询超时，请缩小日期范围".into(),
        )
        .into_response(),
    }
}

pub(super) fn too_large() -> WebError {
    WebError(
        StatusCode::PAYLOAD_TOO_LARGE,
        "结果超出读取预算，请缩小日期范围；未返回部分统计".into(),
    )
}

/// 同时限制行数和结构化文档字节，不能读完大集合再发现超限。
pub(super) struct ReadBudget {
    rows: usize,
    bytes: usize,
}

impl ReadBudget {
    pub(super) fn new(limits: &WebLimits) -> Self {
        Self {
            rows: limits.max_rows,
            bytes: limits.max_response_bytes,
        }
    }

    pub(super) fn document(&mut self, text: &str) -> Result<Value, WebError> {
        self.rows = self.rows.checked_sub(1).ok_or_else(too_large)?;
        self.bytes = self.bytes.checked_sub(text.len()).ok_or_else(too_large)?;
        Ok(serde_json::from_str(text)?)
    }
}

struct ResponseBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl std::io::Write for ResponseBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("响应超过预算"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Response, WebError> {
    let mut buffer = ResponseBuffer {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut buffer, value)
        .map_err(|e| if e.is_io() { too_large() } else { e.into() })?;
    Ok(([(header::CONTENT_TYPE, "application/json")], buffer.bytes).into_response())
}

#[derive(Debug)]
pub(super) struct WebError(pub(super) StatusCode, pub(super) String);

impl<E: Into<BoxError>> From<E> for WebError {
    fn from(error: E) -> Self {
        let error = error.into();
        tracing::error!("只读取数失败：{error}");
        if let Some(sqlx::Error::Database(db)) = error.downcast_ref::<sqlx::Error>()
            && db.code().is_some_and(|code| code == "3024")
        {
            return Self(
                StatusCode::GATEWAY_TIMEOUT,
                "数据库查询超时，请缩小日期范围".into(),
            );
        }
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "读取失败，请查看后端日志".into(),
        )
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_and_byte_budgets_reject_instead_of_truncating() {
        let limits = WebLimits {
            concurrency: 1,
            scan_concurrency: 1,
            max_response_bytes: 6,
            max_rows: 1,
            query_timeout_secs: 1,
        };
        let mut budget = ReadBudget::new(&limits);
        assert_eq!(budget.document("{}").unwrap(), json!({}));
        assert_eq!(
            budget.document("{}").unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            ReadBudget::new(&limits).document("{invalid").unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        // 预算按 UTF-8 字节而不是中文字数，也包括 JSON 引号。
        assert!(bounded_json(&"中文", 8).is_ok());
        assert_eq!(
            bounded_json(&"中文", 7).unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}
