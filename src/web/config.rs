//! 只读工作台自己的配置 —— **跑批不知道 webUI 存在**，所以 webUI 的配置也不住在
//! `config.rs` 里。
//!
//! 它只解析自己要的那四节（`mysql` / `log` / `web` / `roster`），不要求模型、OSS 凭据、
//! 跑批的并发关系，**也不要 `ingest.raw_root`** —— 原文下钻改读
//! `b_merchant_group_event.source_messages` 之后，它一个文件都不碰。
//! 少一份密钥就是少一条泄露路径，而且只读入口能在跑批那套配置齐不齐之前先起来。
//!
//! ⚠️ `[roster]` 是**只有工作台读**的一节：跑批那份 `Config` 里没有它，
//! 多出来的键 serde 直接忽略，两个进程共用一份 `config.toml` 仍然成立。
//!
//! 文件读取与 0600 权限检查跟跑批共用 `config` 的那一份，不重写一遍。

use crate::config::{
    LogConfig, MysqlConfig, MysqlSecrets, SET_SESSION_TZ, load, require_owner_only,
};
use serde::Deserialize;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use std::{path::Path, time::Duration};

/// 只读入口只解析自己的运行参数，不要求模型、OSS 凭据或跑批并发关系。
#[derive(Deserialize)]
pub struct WebConfig {
    pub mysql: MysqlConfig,
    pub log: LogConfig,
    pub web: WebLimits,
    pub roster: RosterConfig,
}

#[derive(Clone, Deserialize)]
pub struct WebLimits {
    pub concurrency: usize,
    pub max_response_bytes: usize,
    pub max_rows: usize,
    pub query_timeout_secs: u64,
    /// 响应缓存的总字节数（见 `cache.rs`）。0 = 关掉缓存。
    pub cache_bytes: usize,
}

/// 外部名册的发现与取数参数 —— 全部必填，代码里没有默认值。
/// 为什么只读工作台可以调外部服务，见 [`crate::web::roster`] 的模块注释。
#[derive(Deserialize)]
pub struct RosterConfig {
    /// Nacos 服务端地址，`scheme://host:port`，**不带 `/nacos` 路径**。
    pub nacos: String,
    /// 命名空间 ID 与分组名。**用的就是 Nacos 默认值时也必须显式写出来** ——
    /// 落进代码当默认值的话，配错了不会在启动时喊，只表现成「解析不到实例」。
    pub namespace: String,
    pub group_name: String,
    /// 商家域与员工域的服务名。写错即启动失败，错误信息带上服务名。
    pub merchant_service: String,
    pub employee_service: String,
    /// 名册（ID → 名字）整体存活多久，到期整张表清空重查。
    pub ttl_secs: u64,
    /// Nacos 与两个业务服务共用的 HTTP 超时。
    pub timeout_secs: u64,
}

#[derive(Deserialize)]
pub struct WebSecrets {
    pub mysql: MysqlSecrets,
    pub roster: RosterSecrets,
}

/// Nacos 的账号密码。跟数据库只读账号同一个 `secrets.toml`，同一份 0600 检查。
///
/// 两个键**必填但可以是空串**：`username = ""` 表示服务端没开鉴权，此时
/// [`crate::web::roster`] 既不登录也不带 `accessToken`。必填是有意的 ——
/// 「忘了写」和「确实不需要」得长得不一样，前者必须启动即崩。
#[derive(Deserialize)]
pub struct RosterSecrets {
    pub username: String,
    pub password: String,
}

pub fn load_from_dir(dir: &Path) -> (WebConfig, WebSecrets) {
    let config: WebConfig = load(&dir.join("config.toml"), false);
    assert!(
        config.mysql.max_connections > 0 && config.mysql.acquire_timeout_secs > 0,
        "MySQL 连接数与等待时间必须大于零"
    );
    assert!(
        config.web.concurrency > 0
            && config.web.max_response_bytes > 0
            && config.web.max_rows > 0
            && config.web.query_timeout_secs > 0,
        "只读查询名额、结果预算与超时必须大于零"
    );
    assert!(
        config.roster.ttl_secs > 0 && config.roster.timeout_secs > 0,
        "名册 TTL 与 HTTP 超时必须大于零"
    );
    let secrets = dir.join("secrets.toml");
    #[cfg(unix)]
    require_owner_only(&secrets);
    (config, load(&secrets, true))
}

/// HTTP 超时之外，数据库本身也限制只读语句执行时间，避免客户端离开后慢查询继续占资源。
pub async fn read_pool(
    cfg: &MysqlConfig,
    url: &str,
    timeout_secs: u64,
) -> crate::Result<MySqlPool> {
    let timeout_ms = timeout_secs
        .checked_mul(1000)
        .filter(|n| *n > 0 && *n <= u32::MAX as u64)
        .ok_or("只读查询超时超出 MySQL 支持范围")?;
    Ok(MySqlPoolOptions::new()
        .max_connections(cfg.max_connections)
        .acquire_timeout(Duration::from_secs(cfg.acquire_timeout_secs))
        .idle_timeout(Duration::from_secs(cfg.idle_timeout_secs))
        .after_connect(move |conn, _| {
            Box::pin(async move {
                sqlx::Executor::execute(&mut *conn, SET_SESSION_TZ).await?;
                sqlx::query("SET SESSION max_execution_time = ?")
                    .bind(timeout_ms)
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(url)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MysqlConfig;

    #[test]
    fn web_startup_needs_only_read_dependencies() {
        let mut value: toml::Value = toml::from_str(include_str!("../../config.toml")).unwrap();
        let table = value.as_table_mut().unwrap();
        for key in ["daily", "extract", "classify", "llm"] {
            table.remove(key);
        }
        // ⚠️ 连 `[ingest]` 一起删掉：只读入口不碰文件系统，配置里有没有 raw_root
        // 都不影响它起来。跑批那份配置仍然必须完整（下面那条 catch_unwind 钉住）。
        table.remove("ingest");
        value["mysql"]["max_connections"] = 1.into();
        let dir = crate::testutil::fresh_root("config", "web-only");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), toml::to_string(&value).unwrap()).unwrap();
        let secret_path = dir.join("secrets.toml");
        std::fs::write(
            &secret_path,
            "[mysql]\nurl = 'mysql://localhost/read_only'\n\
             [roster]\nusername = 'nacos'\npassword = 'nacos'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let (cfg, secrets) = load_from_dir(&dir);
        assert_eq!(cfg.mysql.max_connections, 1);
        assert_eq!(secrets.mysql.url, "mysql://localhost/read_only");
        assert_eq!(secrets.roster.username, "nacos");
        assert!(
            std::panic::catch_unwind(|| crate::config::load_from_dir(&dir)).is_err(),
            "跑批仍必须具备完整配置"
        );
        // `[roster]` 的键同样没有默认值 —— 少一个必须是崩，不是悄悄走一个谁都没写过的值。
        let mut holed = value.clone();
        holed["roster"].as_table_mut().unwrap().remove("group_name");
        std::fs::write(dir.join("config.toml"), toml::to_string(&holed).unwrap()).unwrap();
        assert!(std::panic::catch_unwind(|| load_from_dir(&dir)).is_err());
        value["web"]["max_rows"] = 0.into();
        std::fs::write(dir.join("config.toml"), toml::to_string(&value).unwrap()).unwrap();
        assert!(std::panic::catch_unwind(|| load_from_dir(&dir)).is_err());
    }

    #[tokio::test]
    #[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
    async fn mysql_read_pool_limits_server_execution_time() {
        let url = std::env::var("CHAT2EVENTS_TEST_DATABASE_URL").unwrap();
        let config = MysqlConfig {
            max_connections: 1,
            acquire_timeout_secs: 2,
            idle_timeout_secs: 240,
        };
        let pool = read_pool(&config, &url, 1).await.unwrap();
        let settings: (u64, String) =
            sqlx::query_as("SELECT @@session.max_execution_time, @@session.time_zone")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(settings, (1000, "+08:00".into()));
        let elapsed = std::time::Instant::now();
        // SLEEP 被中断时 MySQL 可返回 1 或查询中断错误；两种都必须在服务器预算内结束。
        let result = sqlx::query_scalar::<_, i32>("SELECT SLEEP(10)")
            .fetch_one(&pool)
            .await;
        assert!(elapsed.elapsed() < Duration::from_secs(3));
        match result {
            Ok(value) => assert_eq!(value, 1),
            Err(error) => assert!(error.as_database_error().is_some()),
        }
        pool.close().await;
    }
}
