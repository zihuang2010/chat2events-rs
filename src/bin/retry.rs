//! 人工重跑 `run_failure` 里还没修好的群 —— 自动挑群、自动定窗口。
//!
//! ```sh
//! # 先空跑看一眼规模和窗口，一个模型请求都不发
//! cargo run --release --bin retry -- . 2026-09-01 --dry-run
//! # 确认没问题再真跑
//! cargo run --release --bin retry -- . 2026-09-01
//! ```
//!
//! `<since>` 夹的是 **`run_date`（跑批日）**，不是数据日 —— 「重跑最近一周失败的」。
//!
//! **和另外两个人工入口的分工**：
//!
//! * `backfill` —— 群和窗口都由人给。挑一个群单跑一遍、或者补一段历史，走它。
//! * `recover`  —— 只补打标，窗口由人给，不重抽。
//! * 本入口     —— 上面两件事的**自动挑活版**：谁还没修好、窗口是哪一段，查库算出来。
//!
//! ⚠️ **会写穿冻结区（承重不变量 1）**，条件和 `backfill` 一样 —— 失败窗口早于
//! 日常窗口起点时，那几天的事实列整体删重写。所以 `--dry-run` 先看一眼是标准动作，
//! 覆盖冻结区时日志里有一条 `warn`。
//!
//! ⚠️ **先停掉覆盖相同群日的日常跑批和重打标**：分类缓存是单写者互斥的，
//! 撞上了会直接报「无法独占分类缓存」。
use chat2events_rs::{Result, boot::Boot, process::daily};

#[tokio::main]
async fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [dir, since] = a
        .get(..2)
        .and_then(|s| <[String; 2]>::try_from(s.to_vec()).ok())
        .expect(
            "用法: retry <config_dir> <since> [--dry-run]   \
             例: retry . 2026-09-01 --dry-run",
        );
    // 只认这一个开关，拼错了当场炸 —— 静默忽略一个没拼对的 `--dry-run`
    // 就是「以为在空跑，实际上真烧了 token」。
    let dry_run = match a.get(2..).unwrap_or(&[]) {
        [] => false,
        [flag] if flag == "--dry-run" => true,
        rest => panic!("只认 --dry-run 这一个开关，收到：{rest:?}"),
    };

    let b = Boot::load(std::path::Path::new(&dir));
    let (extract_llm, classify_llm) = b.llms()?;
    let pool = b.pool().await?;
    daily::retry(
        &b.config,
        &extract_llm,
        &classify_llm,
        &pool,
        &b.secrets.oss,
        since.parse()?,
        dry_run,
    )
    .await
}
