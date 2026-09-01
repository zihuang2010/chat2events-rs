//! 拉取的失败类型 —— **两种失败的处置方式不同，所以必须在类型上分开**。
//!
//! 跟 [`crate::ingest::IngestError`] 一个规矩：`Round` 整轮退出、`Room` 该群跳过
//! 一行不写（承重不变量 3）、`Transient` 只在 `download` 内部活着，不出那个文件。

use std::fmt;

/// 两种失败的**处置方式不同**，所以必须在类型上分开（跟 `IngestError` 同一规矩）。
/// 曾经这条分界靠「错误出现的位置」表达（走 `?` = 整轮死、进 done 向量 = 群级），
/// 约定完全隐式 —— 新增失败路径没有任何东西提醒你选对通道。
#[derive(Debug)]
pub enum MirrorError {
    /// 索引表查不到 / HTTP 客户端起不来 —— 不是某个群的事，整轮失败。
    Round(String),
    /// 某群某月拉取或校验失败 —— 该群本轮不参与跑批（承重不变量 3）。
    Room(String),
    /// 端侧的临时状况（连接类 / 超时 / 5xx / 429 / 408）—— **还能靠重试救回来**。
    ///
    /// 它只活在 `download` 的 `download_one` 和 `download_with_retry` 之间：重试次数用完就降级成
    /// [`Self::Room`]，不出那个函数。分成独立变体是因为「能不能重试」和「谁失败了」
    /// 是两个正交的问题 —— 校验失败也是 `Room`，但重试一万次还是同一份旧副本。
    Transient(String),
}

impl fmt::Display for MirrorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Round(m) | Self::Room(m) | Self::Transient(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for MirrorError {}

/// 索引表连不上/查不动/行的形状不对 —— 上游侧的整轮问题。
impl From<sqlx::Error> for MirrorError {
    fn from(e: sqlx::Error) -> Self {
        Self::Round(format!("索引表：{e}"))
    }
}

/// 下载中的 HTTP 错误 —— 连不上 / 超时 / 读 body 断了，全是重试能救的那类。
///
/// ⚠️ URL 拼错也会走这里（`send()` 返回 builder error），白白重试两次。
/// 不为它单开一条分支：那是配置错误，每个群都会撞上，第一个群就够吼醒你了。
impl From<reqwest::Error> for MirrorError {
    fn from(e: reqwest::Error) -> Self {
        Self::Transient(format!("下载：{e}"))
    }
}

/// 本地读写错误发生在单个月文件上 —— 群级。
impl From<std::io::Error> for MirrorError {
    fn from(e: std::io::Error) -> Self {
        Self::Room(format!("本地文件：{e}"))
    }
}

/// 校验类失败的文案（字节数 / 行边界 / 记录数）—— 全部是群级。
impl From<String> for MirrorError {
    fn from(m: String) -> Self {
        Self::Room(m)
    }
}

pub(super) type Result<T> = std::result::Result<T, MirrorError>;
