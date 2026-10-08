use super::*;
use crate::testutil::scripted;
use serde_json::{Value, json};

const MERCHANT: &str = "merchant-service";
const EMPLOYEE: &str = "employee-service";
const LOGIN: &str = "/nacos/v1/auth/login";
const LIST: &str = "/nacos/v1/ns/instance/list";

fn login(ttl: u64, token: &str) -> (&'static str, u16, Value) {
    (LOGIN, 200, json!({"accessToken": token, "tokenTtl": ttl}))
}

fn hosts(hosts: Value) -> (&'static str, u16, Value) {
    (LIST, 200, json!({"cacheMillis": 10000, "hosts": hosts}))
}

/// 服务端把令牌作废了（Nacos 重启 / 改密 / 清会话）。
fn denied() -> (&'static str, u16, Value) {
    (LIST, 403, json!({"status": 403}))
}

fn config(base: &str) -> NacosConfig {
    NacosConfig {
        nacos: base.into(),
        namespace: "public".into(),
        group_name: "DEFAULT_GROUP".into(),
        merchant_service: MERCHANT.into(),
        employee_service: EMPLOYEE.into(),
        timeout_secs: 3,
    }
}

fn secrets() -> NacosSecrets {
    NacosSecrets {
        username: "nacos".into(),
        password: "s3cret".into(),
    }
}

/// 运行期刷新失败**不清空**上一次的实例列表 —— 启动期拉不到是崩，运行期拉不到是
/// 沿用。两者的差别就在这张表上，清空了页面会从「显示名字」退成「显示 ID」。
#[tokio::test]
async fn a_failed_refresh_keeps_the_previous_instance_list() {
    let healthy = json!([{"ip": "10.0.0.1", "port": 8080, "healthy": true}]);
    // 脚本只够一轮；第二轮时假服务端已经退出，连接直接被拒。
    let (base, server) = scripted(vec![
        login(18000, "tok"),
        hosts(healthy.clone()),
        hosts(healthy),
    ]);
    let cfg = config(&base);
    let mut nacos = Nacos::new(&cfg, &secrets()).unwrap();
    let services = vec![MERCHANT.to_owned(), EMPLOYEE.to_owned()];
    let hosts = Hosts::default();
    nacos.refresh(&services, &hosts).await.unwrap();
    server.join().unwrap();

    let error = nacos
        .refresh(&services, &hosts)
        .await
        .unwrap_err()
        .to_string();
    // ⚠️ 令牌在查询串里，而这条错误会原样进 `warn!` 再进 journald。
    // reqwest 的 Display 默认要拼一句 `for url (...)` —— 剥掉了才看得到这条通过。
    assert!(!error.contains("tok"), "令牌泄漏进日志了：{error}");
    let kept = hosts.read().unwrap();
    assert_eq!(kept[MERCHANT], ["http://10.0.0.1:8080"]);
    assert_eq!(kept[EMPLOYEE], ["http://10.0.0.1:8080"]);
}

/// 服务端提前作废令牌（403）→ **下一轮立刻重新登录**，而不是干等到 `tokenTtl`
/// 的九成走完。默认 ttl 18000s，等下去就是最长 4.5 小时实例列表不再更新，
/// 其间 `pick()` 一直在发可能已经下线的地址。
#[tokio::test]
async fn a_token_revoked_by_the_server_forces_an_immediate_relogin() {
    let healthy = json!([{"ip": "10.0.0.1", "port": 8080, "healthy": true}]);
    let (base, server) = scripted(vec![
        login(18000, "tok-old"),
        hosts(healthy.clone()),
        denied(),
        login(18000, "tok-new"),
        hosts(healthy.clone()),
        hosts(healthy),
    ]);
    let cfg = config(&base);
    let mut nacos = Nacos::new(&cfg, &secrets()).unwrap();
    let services = vec![MERCHANT.to_owned(), EMPLOYEE.to_owned()];
    let hosts = Hosts::default();
    assert!(nacos.refresh(&services, &hosts).await.is_err());
    nacos.refresh(&services, &hosts).await.unwrap();

    let requests = server.join().unwrap();
    let logins = requests.iter().filter(|(l, _)| l.contains(LOGIN)).count();
    assert_eq!(logins, 2, "403 之后必须重新登录：{requests:?}");
    let last = requests.last().unwrap();
    assert!(last.0.contains("accessToken=tok-new"), "{last:?}");
}

/// 协议：登录请求的形状、令牌作为**查询参数**、只有健康实例被选中，
/// 且两个服务共用一次登录（不是每次请求都登录）。
#[tokio::test]
async fn login_shape_token_as_query_parameter_and_only_healthy_instances() {
    let (base, server) = scripted(vec![
        login(18000, "tok-abc"),
        hosts(json!([{"ip": "10.0.0.1", "port": 8080, "healthy": true}])),
        hosts(json!([
            {"ip": "10.0.0.2", "port": 9090, "healthy": false},
            {"ip": "10.0.0.3", "port": 9090, "healthy": true},
        ])),
    ]);
    let discovery = Discovery::start(&config(&base), &secrets()).await.unwrap();
    assert_eq!(discovery.pick(MERCHANT).unwrap(), "http://10.0.0.1:8080");
    // 不健康那台必须一次都选不到 —— 随机选也不能选到它。
    assert_eq!(discovery.pick(EMPLOYEE).unwrap(), "http://10.0.0.3:9090");

    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3, "两个服务只登录一次：{requests:?}");
    let (line, body) = &requests[0];
    assert_eq!(line, &format!("POST {LOGIN} HTTP/1.1"));
    assert_eq!(body, "username=nacos&password=s3cret");
    for (line, body) in &requests[1..] {
        assert!(line.starts_with(&format!("GET {LIST}?")), "{line}");
        assert!(line.contains("accessToken=tok-abc"), "{line}");
        assert!(line.contains("healthyOnly=true"), "{line}");
        assert!(line.contains("groupName=DEFAULT_GROUP"), "{line}");
        assert!(line.contains("namespaceId=public"), "{line}");
        assert!(body.is_empty(), "实例查询是 GET，不该有正文");
    }
    assert!(requests[1].0.contains(&format!("serviceName={MERCHANT}")));
    assert!(requests[2].0.contains(&format!("serviceName={EMPLOYEE}")));
}

/// 协议：服务端没开鉴权时 `[roster].username` 留空 ⇒ **一次登录都不发**，
/// 实例查询里也没有 `accessToken` 参数。脚本里没有 `LOGIN` 这一条，
/// 真发了登录请求假 Nacos 会当场 panic。
#[tokio::test]
async fn an_unauthenticated_nacos_skips_login_and_the_token_parameter() {
    let healthy = json!([{"ip": "10.0.0.1", "port": 8080, "healthy": true}]);
    let (base, server) = scripted(vec![hosts(healthy.clone()), hosts(healthy)]);
    let anonymous = NacosSecrets {
        username: String::new(),
        password: String::new(),
    };
    Discovery::start(&config(&base), &anonymous).await.unwrap();

    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2, "只该有两次实例查询：{requests:?}");
    for (line, _) in &requests {
        assert!(line.starts_with(&format!("GET {LIST}?")), "{line}");
        assert!(!line.contains("accessToken"), "{line}");
    }
}

/// 协议：令牌按 `tokenTtl` 算的提前量到了才重新登录，新令牌立刻用上。
#[tokio::test]
async fn an_expiring_token_is_renewed_before_the_next_lookup() {
    let healthy = json!([{"ip": "10.0.0.1", "port": 8080, "healthy": true}]);
    // 第一次登录的 ttl = 0 ⇒ 提前量也是 0 ⇒ 下一轮刷新必须重新登录。
    let (base, server) = scripted(vec![
        login(0, "tok-old"),
        hosts(healthy.clone()),
        hosts(healthy.clone()),
        login(18000, "tok-new"),
        hosts(healthy.clone()),
        hosts(healthy),
    ]);
    let cfg = config(&base);
    let mut nacos = Nacos::new(&cfg, &secrets()).unwrap();
    let services = vec![MERCHANT.to_owned(), EMPLOYEE.to_owned()];
    let hosts = Hosts::default();
    nacos.refresh(&services, &hosts).await.unwrap();
    nacos.refresh(&services, &hosts).await.unwrap();

    let requests = server.join().unwrap();
    let logins: Vec<_> = requests.iter().filter(|(l, _)| l.contains(LOGIN)).collect();
    assert_eq!(logins.len(), 2, "过期后要重新登录一次：{requests:?}");
    let tokens: Vec<_> = requests
        .iter()
        .filter(|(l, _)| l.contains(LIST))
        .map(|(l, _)| l.contains("accessToken=tok-new"))
        .collect();
    assert_eq!(
        tokens,
        [false, false, true, true],
        "重登之后必须立刻换用新令牌：{requests:?}"
    );
}

/// 启动期：解析不到实例就**退出**，错误信息要指得出是哪一项。
#[tokio::test]
async fn an_unknown_service_name_fails_startup_with_a_locatable_error() {
    let (base, server) = scripted(vec![login(18000, "tok"), hosts(json!([]))]);
    let error = Discovery::start(&config(&base), &secrets())
        .await
        .err()
        .expect("解析不到实例必须启动失败")
        .to_string();
    assert!(error.contains(MERCHANT), "{error}");
    assert!(error.contains("roster.namespace"), "{error}");
    assert!(error.contains("roster.group_name"), "{error}");
    server.join().unwrap();
}

/// 服务端地址写错 —— 连登录都发不出去，错误要点名是哪个配置键。
#[test]
fn a_malformed_endpoint_names_the_configuration_key() {
    for bad in ["nacos.internal:8848", "http://host:8848/nacos", ""] {
        let mut cfg = config("http://127.0.0.1:8848");
        cfg.nacos = bad.into();
        let error = Nacos::new(&cfg, &secrets()).err().unwrap().to_string();
        assert!(error.contains("roster.nacos"), "{bad}：{error}");
    }
}
