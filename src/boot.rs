//! 进程引导 —— 读配置 · 起日志 · 建连接池 · 建两队模型。
//!
//! **六个入口曾经各抄一遍这四件事**（`main` · `backfill` · `recover` · `recompute` ·
//! `taxonomy` · `smoke`），没有任何模块拥有它。抄写本身不贵，贵的是抄漏：
//! `main.rs` 里那行「两队各打一行」的启动日志**只有 `main` 有**，而 `backfill`
//! 建了同样的两队 `Llm`、走同样的 `daily::run_span`，一行都没打 —— 补跑那一轮
//! 用的是哪两个模型，日志里查不到。
//!
//! 所以 [`Boot::llms`] 把「建两队」和「打那行日志」焊在同一个函数里：
//! 想拿到两队就一定会打，忘不掉。这是本模块存在的**全部**理由，别往里加别的。
//!
//! ⚠️ **`webui` 不用它。** 只读工作台走 `web::config::load_from_dir`，返回的是
//! `(WebConfig, WebSecrets)` —— 与这里的 `(Config, Secrets)` 是两个类型，
//! 连接池也多设一个 `max_execution_time`。硬套一个 `Boot` 只会造出一个
//! 两边都不合身的接口，那正是 `CLAUDE.md` 说的「抽象等到有真实的第二个用例」。
//!
//! ⚠️ **只给 `src/bin/` 用，不进 lib 的测试。** [`config::init_logging`] 走
//! `tracing_subscriber` 的 `.init()`，全局且二次调用会 panic。

use crate::{
    Result,
    config::{self, Config, Secrets},
    llm::Llm,
};
use sqlx::MySqlPool;
use std::path::Path;

/// 一个进程起手拿到的两份配置。字段公开 —— 各入口要读自己那几个键
/// （`backfill` 读 `ingest.lookback_days`、`recompute` 读 `mysql.max_connections`…），
/// 全部包一遍访问器只是把 `Config` 抄第二遍。
pub struct Boot {
    pub config: Config,
    pub secrets: Secrets,
}

impl Boot {
    /// 配置目录取第一个命令行参数，缺省当前目录（生产传 `/etc/chat2events`）。
    pub fn from_args() -> Self {
        Self::load(&config::dir_from_args())
    }

    /// 读两份配置并起日志。**读不到、字段缺、类型不对一律 panic** ——
    /// 配置错误要在进程起来的第一秒暴露，见 `config` 的模块注释。
    pub fn load(dir: &Path) -> Self {
        let (config, secrets) = config::load_from_dir(dir);
        config::init_logging(&config.log);
        Self { config, secrets }
    }

    /// 连接池。`config::mysql_pool` 用的是 `.connect()` 不是 `.connect_lazy()` ——
    /// 库连不上就在这里炸，别等抽完了才发现进不去。
    pub async fn pool(&self) -> Result<MySqlPool> {
        let pool = config::mysql_pool(&self.config.mysql, &self.secrets.mysql.url).await?;
        tracing::info!(
            max_connections = self.config.mysql.max_connections,
            "MySQL 连接池就绪"
        );
        Ok(pool)
    }

    /// **抽取队 + 打标队，并打那行启动日志。**
    ///
    /// 抽取和打标是两个不同的模型，而下游收的是两个同类型的 `&Llm` ——
    /// 传反了编译过、测试也过。这行日志是第一秒就能看见它的地方：
    /// 抽取那行的 `max_tokens` 该是万级，打标那行该是千级，反了一眼就认得出来。
    ///
    /// **返回两队的唯一途径就是这个函数**，所以日志漏不掉。这是它存在的理由 ——
    /// 拆成两个 `extract_llm()` + `classify_llm()` 就又回到了「记得自己打」。
    pub fn llms(&self) -> Result<(Llm, Llm)> {
        let c = &self.config.llm;
        tracing::info!(
            extract_model = %c.extract.model,
            extract_base_url = %c.extract.base_url,
            extract_max_tokens = c.extract.max_tokens,
            classify_model = %c.classify.model,
            classify_base_url = %c.classify.base_url,
            classify_max_tokens = c.classify.max_tokens,
            reasoning_effort = ?c.reasoning_effort,
            "启动"
        );
        Ok((self.extract_llm()?, self.classify_llm()?))
    }

    /// 只要抽取那一队（`smoke` 的冒烟只发抽取请求）。
    pub fn extract_llm(&self) -> Result<Llm> {
        Llm::new(
            &self.config.llm,
            &self.config.llm.extract,
            self.secrets.llm.api_key.clone(),
        )
    }

    /// 只要打标那一队（`recover` / `recompute` / `taxonomy review` 只走 ⑤ 这条线）。
    pub fn classify_llm(&self) -> Result<Llm> {
        Llm::new(
            &self.config.llm,
            &self.config.llm.classify,
            self.secrets.llm.api_key.clone(),
        )
    }
}
