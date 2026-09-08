//! ⑤ 的词表归纳 —— **人工触发，只产词表**，不写 `b_merchant_group_event`、
//! 不参与跑批、失败无所谓 —— 跑批不知道它存在。
//!
//! ⚠️ **机器归纳两条路都放弃了**（A：LLM map-reduce，2026-09-02 删；
//! B：embedding + HDBSCAN，2026-09-03 删）。
//! **词表现在从头到尾由人手写**，这个入口负责的是人手写之后的两步。
//!
//! ```sh
//! # 1. 看语料：库里有哪些说法、各多少条（写词表前的参考，不产草稿）
//! cargo run --release --example taxonomy -- . summaries 2026-08-01 2026-08-31 | head -200
//!
//! # 2. ★ 人手写 taxonomy_v2.toml：逐个 [[types]] 填 type_id / parent_name / name / description
//!
//! # 3. 试打，看未分类率和空类数，不行就回第 2 步改
//! cargo run --release --example taxonomy -- . review taxonomy_v2.toml 2026-08-01 2026-08-31
//!
//! # 4. 定稿转 SQL，人工执行
//! cargo run --release --example taxonomy -- . emit-sql taxonomy_v2.toml > taxonomy_v2.sql
//! mysql ... < taxonomy_v2.sql
//!
//! # 5. 把 classify::CURRENT_VERSION 改成 "v2"，重新编译部署，再跑 examples/recompute.rs
//! ```
//!
//! 第 3 步不可省 —— 词表定下后不再漂移，而 `event_type` 一旦逐日漂移，
//! 就没有报表能建在这个维度上（`CONTEXT.md`）。
use chat2events_rs::{Result, config, llm::Llm, taxonomy};
use std::path::Path;

#[tokio::main]
async fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let usage = "用法:\n  \
        taxonomy <config_dir> summaries <since> <until>            去重后的 summary 及条数，高频在前\n  \
        taxonomy <config_dir> review    <draft.toml> <since> <until> [out_dir]\n  \
        taxonomy <config_dir> emit-sql  <draft.toml>\n\
        （词表由人手写，没有归纳子命令 —— 见顶注）";
    let (dir, cmd) = (a.first().expect(usage), a.get(1).expect(usage).as_str());

    // emit-sql 不连库、不建模型 —— 它只是把一个文本文件翻译成另一个。
    // **所以它也不该在 `load_from_dir` 后面**：那会让「把 TOML 翻成 SQL」要求
    // secrets.toml 存在且权限正好 0600，而这一步一个密钥都用不上。
    if cmd == "emit-sql" {
        let d = taxonomy::Draft::load(Path::new(a.get(2).expect(usage)))?;
        print!("{}", taxonomy::to_sql(&d)?);
        return Ok(());
    }

    let (cfg, secrets) = config::load_from_dir(Path::new(dir));
    config::init_logging(&cfg.log);

    let pool = config::mysql_pool(&cfg.mysql, &secrets.mysql.url).await?;
    // 两个子命令的日期位置不同（`review` 前面还夹着 draft.toml），各分支各取各的。
    let dates = |i: usize| -> Result<(chrono::NaiveDate, chrono::NaiveDate)> {
        Ok((
            a.get(i).expect(usage).parse()?,
            a.get(i + 1).expect(usage).parse()?,
        ))
    };

    match cmd {
        // 人手写词表前必看：**有哪些说法、各多少条**。制表符分隔，方便 `| head` 或重定向。
        // 不建 `Llm` —— 这一步一次模型请求都不发。
        "summaries" => {
            let (since, until) = dates(2)?;
            for (s, n) in taxonomy::summaries(&pool, since, until).await? {
                println!("{n}\t{s}");
            }
            Ok(())
        }
        "review" => {
            let (since, until) = dates(3)?;
            let out = Path::new(a.get(5).map_or(".", String::as_str));
            // 试打走的是 ⑤ 的打标路径，用打标那一队的模型。
            let llm = Llm::new(&cfg.llm, &cfg.llm.classify, secrets.llm.api_key)?;
            let d = taxonomy::Draft::load(Path::new(a.get(2).expect(usage)))?;
            let sums = taxonomy::summaries(&pool, since, until).await?;
            let p = taxonomy::review_draft(&cfg, &llm, &d, &sums, out).await?;
            tracing::info!(path = %p.display(), "审阅报告已落盘");
            Ok(())
        }
        other => Err(format!("不认识的子命令「{other}」\n{usage}").into()),
    }
}
