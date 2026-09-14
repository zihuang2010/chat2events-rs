//! 补跑历史 —— 把过去一段日期**整体**重抽一遍，用来给 ⑤ 的词表归纳攒 event。
//!
//! ```sh
//! cargo run --release --bin backfill -- . 2026-08-01 2026-08-31
//! ```
//!
//! **一趟跑完，不是循环喂 `daily::run`。** `Window::new` 的窗口是 `[T-(N+1), T-2]`，
//! 连着喂 31 个 run_date 会让相邻窗口两两重叠 —— 同一天被抽两遍，白烧一倍 token。
//! 这里走 [`Window::span`]，每条消息恰好进一次模型。
//!
//! ⚠️ **这会写穿冻结区（承重不变量 1）。** `store::stray_days` 校验的是「事件落在
//! 传进来的窗口内」，窗口一宽守卫就跟着放宽。冻结区的事实列本来只在这种人工授权的
//! 补跑里才允许重来 —— 所以下面那行 `warn!` 是必须的，不能让补跑在日志上跟日常
//! 跑批长得一模一样。
//!
//! 其余一切与生产同一条路径：同样的 `daily::run_span`、同样的失败隔离、同样的事务。
use chat2events_rs::{Result, boot::Boot, process::daily, window::Window};

#[tokio::main]
async fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [dir, since, until] = a
        .get(..3)
        .and_then(|s| <[String; 3]>::try_from(s.to_vec()).ok())
        .expect(
            "用法: backfill <config_dir> <since> <until>   例: backfill . 2026-08-01 2026-08-31",
        );

    let b = Boot::load(std::path::Path::new(&dir));
    let cfg = &b.config;

    // 区间倒挂 / 日期写错 → `Window::span` 直接 panic，跟配置错误一个待遇
    let w = Window::span(since.parse()?, until.parse()?);

    // `run_date` 只进 `run_failure.run_date`（「哪一次跑批出的事」），不参与窗口计算。
    // 用今天：补跑失败的账要记在今天这次人工操作上，不是记在被补的那一天。
    let run_date = chrono::Local::now().date_naive();
    let daily_w = Window::new(run_date, cfg.ingest.lookback_days);
    if w.since() < daily_w.since() {
        tracing::warn!(
            since = %w.since(),
            until = %w.until(),
            frozen_before = %daily_w.since(),
            "补跑窗口覆盖冻结区：{} 之前的事实列将被整体删重写。\
             这是人工授权的重来，不是日常跑批 —— 确认这是你要的。",
            daily_w.since()
        );
    }

    // 补跑走整条流水线，两队都要。`llms()` 顺带打那行启动日志 ——
    // 此前这里建了同样的两队却一行没打，补跑用的是哪两个模型日志里查不到。
    let (extract_llm, classify_llm) = b.llms()?;
    let pool = b.pool().await?;
    daily::run_span(
        cfg,
        &extract_llm,
        &classify_llm,
        &pool,
        &b.secrets.oss,
        run_date,
        w,
    )
    .await
}
