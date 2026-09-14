//! 只读工作台的**资源上限**与错误映射 —— 三道闸，全部「拒绝」而不是「截断」。
//!
//! 1. [`admit`]：并发名额 + 整请求超时。拿不到名额就 503，不排队。
//! 2. [`ResponseBudget`]：边读边扣行数与字节，**不能读完大集合再发现超限**；
//!    超出字节上限就 413。读进来的那一半和写出去的那一半是**同一份预算、同一个缓冲**。
//! 3. [`WebError`]：所有失败在这里统一成状态码，日志留全文、响应只给一句话。
//!
//! 超限一律是显式错误 —— 截断会让主管拿到一个偏小但看起来正常的数字，
//! 跟承重不变量 4「绝不用 0 表示没算出来」同一个形状。
//!
//! ## 为什么读进来和写出去是同一个东西
//!
//! 此前是两个类型：`ReadBudget` 扣行数与字节、顺手把数据库已经渲染好的 JSON
//! **解析成 `Value`**，`ResponseBuffer` 再把那棵值树**原样序列化回去**。
//! 解析唯一用到的信息是字符串长度，而峰值内存是三份同时活着 —— 实测值树约为
//! 原文的 **6.8 倍**。真实内存上限因此是「并发名额 × `max_response_bytes` × 6.8」，
//! 而 `max_rows` 是行数闸不是内存闸，那个乘积没有写在任何配置项上。
//!
//! 现在预渲染文档（群日记录、事件明细、单事件、原文）**直推字节缓冲不解析**，
//! 这条路上的峰值从约 7 倍降到约 1 倍（就是响应体本身）。聚合接口的数字是 Rust 算的，
//! 走 [`ResponseBudget::value`] 序列化，那条路必须留。
//!
//! ⚠️ **接受失去原先顺带做的 JSON 格式校验。** 它校验的是数据库自己刚用
//! `JSON_OBJECT` 生成的东西；真正的契约边界是前端的响应校验（`webui/src/api/client.ts`
//! 每个响应都过一遍 zod），一直在那。**不加 `debug_assert!`** —— 这是一个决定，
//! 不是疏忽：加一个在 release 里会蒸发的断言，只会让它看起来像疏忽。

use super::config::WebLimits;
use super::state::WebState;
use crate::BoxError;
use axum::{
    Json,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::json;
use std::time::Duration;

/// 超过它就打一条 warn。**慢查询要在自己的日志里看得见** —— 只靠数据库的慢查询日志，
/// 等有人想起来去翻的时候，它已经慢了三个月。取值贴着「人还愿意等」而不是「超时线」：
/// 超时是护栏，这个是体检。
const SLOW_REQUEST: Duration = Duration::from_millis(800);

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
    // 路径和查询串一起记：慢下来的往往是某一段日期范围，光有路径看不出是哪次。
    // ⚠️ 只记 URI，不记响应体 —— 那里面是未脱敏的客户正文。
    let uri = request.uri().to_string();
    let started = std::time::Instant::now();
    // 名额覆盖取数与响应序列化；等待超时取消数据库读取。
    let response = match tokio::time::timeout(
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
    };
    let elapsed = started.elapsed();
    if elapsed >= SLOW_REQUEST {
        tracing::warn!(
            uri, ms = elapsed.as_millis(), status = %response.status(),
            "只读请求超过 {SLOW_REQUEST:?}"
        );
    }
    response
}

pub(super) fn too_large() -> WebError {
    WebError(
        StatusCode::PAYLOAD_TOO_LARGE,
        "结果超出读取预算，请缩小日期范围；未返回部分统计".into(),
    )
}

/// 一次响应的行数与字节预算，**同时就是它的输出缓冲**。
///
/// 三个追加入口，调用方只需要知道一句话：**有文本就推、有结构就序列化，
/// 超了都是 413，绝不截断**。
///
///   * [`Self::document`] —— 数据库已经渲染好的 JSON 文档，直推不解析，扣一行；
///   * [`Self::value`] —— Rust 侧构造的结构，序列化进同一个缓冲；
///   * [`Self::frame`] —— 把前两者串成一个响应体的 JSON 框架字符（`{"rows":[` 之类）。
///     签名收 `&'static str`，**只有编译期字面量进得来**，外部数据没有这条路。
pub(super) struct ResponseBudget {
    rows: usize,
    limit: usize,
    out: Vec<u8>,
}

impl ResponseBudget {
    pub(super) fn new(limits: &WebLimits) -> Self {
        Self {
            rows: limits.max_rows,
            limit: limits.max_response_bytes,
            out: Vec::new(),
        }
    }

    /// 数据库已经渲染好的文档 —— **直推字节，不解析**。行数在这里扣，
    /// 字节由缓冲上限统一管（文档进的就是响应体，两份计数没有意义）。
    pub(super) fn document(&mut self, text: &str) -> Result<(), WebError> {
        self.rows = self.rows.checked_sub(1).ok_or_else(too_large)?;
        self.push(text.as_bytes())
    }

    /// Rust 侧构造的结构 —— 聚合接口的数字是 Rust 算的，这条路必须留。
    pub(super) fn value(&mut self, value: &impl Serialize) -> Result<(), WebError> {
        serde_json::to_writer(&mut *self, value)
            .map_err(|e| if e.is_io() { too_large() } else { e.into() })
    }

    /// 响应体的 JSON 框架字符。**只收字面量**（见类型文档）。
    pub(super) fn frame(&mut self, literal: &'static str) -> Result<(), WebError> {
        self.push(literal.as_bytes())
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), WebError> {
        if bytes.len() > self.limit.saturating_sub(self.out.len()) {
            return Err(too_large());
        }
        self.out.extend_from_slice(bytes);
        Ok(())
    }

    pub(super) fn finish(self) -> Response {
        ([(header::CONTENT_TYPE, "application/json")], self.out).into_response()
    }
}

/// 给 [`ResponseBudget::value`] 用 —— `serde_json` 的 IO 错误在那边翻成 413。
impl std::io::Write for ResponseBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.push(bytes)
            .map(|()| bytes.len())
            .map_err(|_| std::io::Error::other("响应超过预算"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 整个响应就是一个 Rust 侧结构时的快捷方式（五个聚合接口）。
pub(super) fn bounded_json(
    value: &impl Serialize,
    limits: &WebLimits,
) -> Result<Response, WebError> {
    let mut budget = ResponseBudget::new(limits);
    budget.value(value)?;
    Ok(budget.finish())
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

    fn limits(bytes: usize, rows: usize) -> WebLimits {
        WebLimits {
            concurrency: 1,
            max_response_bytes: bytes,
            max_rows: rows,
            query_timeout_secs: 1,
            cache_bytes: 0,
        }
    }

    #[test]
    fn row_and_byte_budgets_reject_instead_of_truncating() {
        // 行数闸：第二条文档没有行数名额了。
        let mut budget = ResponseBudget::new(&limits(64, 1));
        budget.document("{}").unwrap();
        assert_eq!(
            budget.document("{}").unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        // 字节闸：文档比缓冲上限长。
        assert_eq!(
            ResponseBudget::new(&limits(6, 9))
                .document("{\"a\":\"bbbb\"}")
                .unwrap_err()
                .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        // 预算按 UTF-8 字节而不是中文字数，也包括 JSON 引号。
        assert!(bounded_json(&"中文", &limits(8, 1)).is_ok());
        assert_eq!(
            bounded_json(&"中文", &limits(7, 1)).unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    /// 预渲染文档与 Rust 侧结构拼在**同一个缓冲**里，框架字符原样在中间。
    #[test]
    fn documents_and_values_share_one_buffer() {
        let mut budget = ResponseBudget::new(&limits(1024, 10));
        budget.frame("{\"total\":").unwrap();
        budget.value(&2i64).unwrap();
        budget.frame(",\"rows\":[").unwrap();
        budget.document("{\"id\":1}").unwrap();
        budget.frame(",").unwrap();
        budget.document("{\"id\":2}").unwrap();
        budget.frame("]}").unwrap();
        assert_eq!(
            String::from_utf8(budget.out).unwrap(),
            "{\"total\":2,\"rows\":[{\"id\":1},{\"id\":2}]}"
        );
    }
}
