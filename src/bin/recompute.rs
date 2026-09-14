//! 词表升版后的重打标 —— **只写标注列**（`event_type` / `taxonomy_version`），
//! 事实列一个字节不动，所以冻结区照样能重打（那正是标注列的定义）。
//!
//! ```sh
//! cargo run --release --bin recompute -- . v1 2026-08-01 2026-08-31 [并发度]
//! ```
//!
//! **所有未命中的摘要交给大模型打标**；与 daily 共用数据库词表、结果校验和持久缓存。
//! 本入口不训练分类器，也不加载或生成类心文件。
//!
//! **并发度默认 8，`mysql.max_connections` 是它的上界**（超了就会有群卡在等连接、
//! `acquire_timeout` 一到整群作废）。墙钟由输出吞吐定：单流 ~110 tok/s，
//! 25 万条摘要约 4.5M 输出 token ≈ 11.4 流·小时 —— 想压进 1 小时要 16。
//!
//! ⚠️ `mysql.max_connections` 现在是 24（为「抽取 + 打标两队各自持连接」抬的，
//! 见 `config.toml` 那条注释），**这个上界因此顺带放宽到了 24** —— 不是为
//! 重打标抬的，是被动继承的。重打标是人工触发、不与 daily 同时跑，所以整池归它用，
//! 但别把 24 当成「实测撑得住 24 路」：那个数没量过。
//!
//! 正确的升版顺序：**插词表 → 改 `classify::CURRENT_VERSION` 重新编译部署 → 跑这个**。
//! 顺序反了的话，明天的跑批会把本次窗口内两天按旧版本打回去 ——
//! 那两天会和其余日期用不同的词表。版本对不上时这里会 `warn`，但不会拦你。
use chat2events_rs::{Result, boot::Boot, process::recompute};
use std::path::Path;

#[tokio::main]
async fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    assert!(
        (4..=5).contains(&a.len()),
        "用法: recompute <config_dir> <version> <since> <until> [并发度]；不再接受种子数"
    );
    let [dir, version, since, until] = a.first_chunk::<4>().expect("参数数量已校验");
    // 缺省 8 = `mysql.max_connections` 今天的值。给大了不报错但会卡在等连接上，
    // 所以这里就地拦住 —— 配置错误要在第一秒暴露。
    let concurrency: usize = a.get(4).map_or(8, |s| s.parse().expect("并发度要是个数"));

    let b = Boot::load(Path::new(dir));
    let cfg = &b.config;

    // 重打标只走 ⑤ 这条线 —— 拿打标那一队的模型。
    let llm = b.classify_llm()?;
    assert!(
        concurrency <= cfg.mysql.max_connections as usize,
        "并发度 {concurrency} 超过 mysql.max_connections = {} —— 会有群卡在等连接、\
         acquire_timeout 一到整群作废。把 config.toml 里那个数一起抬上去",
        cfg.mysql.max_connections
    );

    let pool = b.pool().await?;
    recompute::run(
        cfg,
        &llm,
        &pool,
        version,
        since.parse()?,
        until.parse()?,
        concurrency,
    )
    .await
}
