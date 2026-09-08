//! webUI 只读旁路：MySQL 一致快照与 ingest 原文读取，不写表、不调用模型。

mod read;
pub use read::serve;

#[cfg(test)]
mod tests;
