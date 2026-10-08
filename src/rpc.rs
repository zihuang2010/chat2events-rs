//! 内部服务调用 —— 内核层。刷新进程（[`crate::process::merchant_sync`]）与只读工作台的名册
//! （[`crate::web::roster`]）都向 Nacos 发现的内部服务 POST 一份 JSON，再按上游统一的
//! `Result<T>` 包装判定成败；**成功判定只在这里定义一份**，两边各抄一份迟早会漂开。
//!
//! 放在 crate 根而不是 `web/` 或 `process/` 下：两类都用到，按 `lib.rs` 顶注的判据归内核。
//! 「服务名 → 实例地址」是 [`crate::nacos`] 的事，这里只管拿到地址之后的那一次调用。
//!
//! 只有这一道**共同**的判定。**请求非空却拿回 `{}` 算不算失败**是调用方自己的规则
//! （刷新进程算、名册不算），不在这里。

use crate::nacos::Discovery;
use serde::{Deserialize, de::DeserializeOwned};

/// 统一包装 `Result` 的成功码。**是 1，不是 0 也不是 200**（`ResultEnum.SUCCESS`）。
///
/// ⚠️ **必须同时要求 `code == 1` 和 `data` 非 null**，两道一起才闭环：
/// 上游「失败时 `data` 是 null 还是 `{}`」没有权威答案（`Result` 来自外部依赖
/// `com.jdd.integration:jdd-common-resultvo`，源码不在手上）。而空 map 是**合法的成功
/// 响应**（查不到的 ID 不放进 map），所以万一失败时 `data` 也是 `{}`，只看 `data`
/// 就会把一批人当成「查无此人」—— 那是失败不该有的待遇。`code` 这一道先拦住它。
pub const RESULT_OK: i64 = 1;

/// 外层包装 `Result<T>`。`code` / `message` 只在失败时有意义。
#[derive(Deserialize)]
struct Envelope<T> {
    code: Option<i64>,
    message: Option<String>,
    data: Option<T>,
}

/// 向 `service` 的某个健康实例 POST 一次 JSON，返回 `data`。
/// `what` 是「哪个域、哪一步、几个编号」，原样进错误文案 —— 那就是失败日志。
///
/// 每次调用各选一次实例：实例列表按 `cacheMillis` 在后台刷新，用最新那份没坏处，
/// 也顺手把负载摊开。`code != 1` 或 `data` 缺席一律是 `Err`，绝不当成「全部查无」。
///
/// 不用 `.json()` / `Response::json()`：reqwest 的 `json` 特性是别的依赖顺手开的，
/// 别新依赖这个隐式开启。
pub async fn post<T: DeserializeOwned>(
    http: &reqwest::Client,
    discovery: &Discovery,
    service: &str,
    path: &str,
    body: String,
    what: &str,
) -> crate::Result<T> {
    let instance = discovery
        .pick(service)
        .ok_or_else(|| format!("{what}：Nacos 尚无服务 `{service}` 的健康实例"))?;
    let response = http
        .post(format!("{instance}{path}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("{what}：调用失败：{e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("{what}：返回 HTTP {status}（{instance}）").into());
    }
    let answer: Envelope<T> =
        serde_json::from_str(&text).map_err(|e| format!("{what}：应答无法解析：{e}"))?;
    Ok(answer
        .data
        .filter(|_| answer.code == Some(RESULT_OK))
        .ok_or_else(|| {
            format!(
                "{what}：应答异常：code={:?}（成功是 {RESULT_OK}） message={:?}",
                answer.code, answer.message
            )
        })?)
}
