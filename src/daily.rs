//! `daily` 进程 —— 每日跑批的编排。
//!
//! 目标形态是 ① → ③④ → ⑤⑥ → ⑦，今天 ①（拉取 + 摄取）和 ②（会话）是真的，
//! ③ 还是 [`crate::extract`] 里的一段冒烟，④⑤⑥⑦ 尚未搬过来。
//!
//! **编排住在这里，不住在 `main`。** `main` 是进程入口，它该做的只有
//! 「读配置、建资源、调 [`run`]」。[`run`] 是个普通 async 函数，测试直接调得到；
//! `#[tokio::main] async fn main()` 调不到。

use crate::{
    Result,
    config::Config,
    extract,
    ingest::{self, IngestError},
    llm::Llm,
    pull,
    window::Window,
};
use sqlx::MySqlPool;

/// 跑一轮。
pub async fn run(config: &Config, llm: &Llm, pool: &MySqlPool) -> Result<()> {
    let w = Window::new(chrono::Local::now().date_naive(), config.ingest.lookback_days);

    // ① 拉取 —— 把 OSS 上的月文件增量同步到本地 raw 区。
    // 索引表查不到是**整轮**失败（`?` 上抛）；单个群拉不下来只是这个群的事。
    let unsynced = pull::pull(config, pool, &w).await?;

    // ①② 摄取 + 会话 —— 运维用的一眼看：扫了多少群、多少条、花了多久
    let t0 = std::time::Instant::now();
    let mut rooms = ingest::list_rooms(&config.ingest.raw_root, &w);
    // **拉取失败的群本轮一行不写**（承重不变量 3）。这一步同时挡住 CDN 陈旧缓存 ——
    // 拿到旧副本 → 字节数不够 → 该群失败，不会拿残缺数据去出指标。
    rooms.retain(|r| !unsynced.contains(r));

    let (mut n_msg, mut ok_rooms, mut failed_rooms) = (0usize, 0usize, 0usize);
    for (corp, room) in &rooms {
        // ⚠️ Conversation 读完就丢，因为 ③ 还没接上 —— 它才是这份数据的消费者。
        //    ③ 一接上，这个 `.msgs.len()` 就变成 `extract(conv)`。
        match ingest::read_room(&config.ingest.raw_root, corp, room, &w) {
            Ok(conv) => {
                ok_rooms += 1;
                n_msg += conv.msgs.len();
            }
            // 上游解析器变了 —— 不是某个群的事，整轮退出，不做兼容层。
            Err(e @ IngestError::Upstream(_)) => return Err(e.into()),
            // 其余都是该群的事：整体跳过、一行不写、**整轮继续**（承重不变量 3）。
            // ⚠️ 拉取失败的群和这里跳过的群将来都要写 `run_failure`，那一步跟着 ⑦ 一起搬。
            Err(e) => {
                failed_rooms += 1;
                tracing::error!(corp = %corp, room = %room, "读取失败，该群本轮跳过：{e}");
            }
        }
    }
    tracing::info!(
        rooms = rooms.len(),
        ok = ok_rooms,
        failed = failed_rooms,
        unsynced = unsynced.len(),
        msgs = n_msg,
        since = %w.since(),
        until = %w.until(),
        secs = t0.elapsed().as_secs_f64(),
        "①② 摄取完成"
    );

    // ③ 抽取 —— 还是冒烟，走的是硬编码样本不是上面那批会话
    extract::smoke(llm).await?;
    Ok(())
}
