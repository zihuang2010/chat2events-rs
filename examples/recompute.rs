//! 词表升版后的重打标 —— **只写标注列**（`event_type` / `taxonomy_version`），
//! 事实列一个字节不动，所以冻结区照样能重打（那正是标注列的定义）。
//!
//! ```sh
//! cargo run --release --example recompute -- . v1 2026-08-01 2026-08-31 [并发度] [种子数]
//! ```
//!
//! **打标走两段式**：先让模型标高频前 `种子数` 条去重摘要（默认 8000），用答案
//! 训练字符 bigram + IDF 的最近类心；剩下的本地贴，**最近与次近之差不到 `margin`
//! 就回落去问模型**（绝不硬塞最近的类）。`种子数 = 0` 退回老路，全部问模型。
//!
//! **`margin` 在 `config.toml` 里，不是命令行参数** —— `daily` 每天也要用同一个值，
//! 两边不一致就会让冻结区和 `[T-2, T-1]` 用两套口径（承重不变量 1）。
//!
//! ⚠️ **配置里那个 margin 是占位不是实测值。** 先跑一遍看日志里那张
//! 「留出集（margin / 覆盖率 / 一致率）」的表：一致率是质量，覆盖率是省下的钱。
//! 一致率不达标就把 `config.toml` 的 margin 调高，或者 `种子数 = 0` 老实全问。
//!
//! 训练出的类心模型写到 `<cache_dir>/<版本>-<指纹>-nearest.json`，
//! **`daily` 下一轮起会自动加载它** —— 那正是两条路口径一致的方式。
//!
//! **并发度默认 8，`mysql.max_connections` 是它的上界**（超了就会有群卡在等连接、
//! `acquire_timeout` 一到整群作废）。墙钟由输出吞吐定：单流 ~110 tok/s，
//! 25 万条摘要约 4.5M 输出 token ≈ 11.4 流·小时 —— 想压进 1 小时要 16，
//! 那就得把 `config.toml` 的 `mysql.max_connections` 一起抬到 16。
//!
//! 正确的升版顺序：**插词表 → 改 `classify::CURRENT_VERSION` 重新编译部署 → 跑这个**。
//! 顺序反了的话，明天的跑批会把 `[T-2, T-1]` 这两天按旧版本打回去 ——
//! 那两天会和其余日期用不同的词表。版本对不上时这里会 `warn`，但不会拦你。
use chat2events_rs::{Result, config, llm::Llm, recompute};
use std::path::Path;

#[tokio::main]
async fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [dir, version, since, until] = a
        .get(..4)
        .and_then(|s| <[String; 4]>::try_from(s.to_vec()).ok())
        .expect("用法: recompute <config_dir> <version> <since> <until> [并发度] [种子数]");
    // 缺省 8 = `mysql.max_connections` 今天的值。给大了不报错但会卡在等连接上，
    // 所以这里就地拦住 —— 配置错误要在第一秒暴露。
    let concurrency: usize = a.get(4).map_or(8, |s| s.parse().expect("并发度要是个数"));
    let seed: usize = a
        .get(5)
        .map_or(8000, |s| s.parse().expect("种子数要是个数"));

    let (cfg, secrets) = config::load_from_dir(Path::new(&dir));
    config::init_logging(&cfg.log);

    let llm = Llm::new(&cfg.llm, secrets.llm.api_key)?;
    assert!(
        concurrency <= cfg.mysql.max_connections as usize,
        "并发度 {concurrency} 超过 mysql.max_connections = {} —— 会有群卡在等连接、\
         acquire_timeout 一到整群作废。把 config.toml 里那个数一起抬上去",
        cfg.mysql.max_connections
    );

    let pool = config::mysql_pool(&cfg.mysql, &secrets.mysql.url).await?;
    recompute::run(
        &cfg,
        &llm,
        &pool,
        &version,
        since.parse()?,
        until.parse()?,
        concurrency,
        seed,
    )
    .await
}
