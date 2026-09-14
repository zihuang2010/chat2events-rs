//! `config` 的测试 —— 拆到这里的理由见 `src/lib.rs` 的组织规则。

use super::*;

#[test]
fn invalid_scheduling_values_are_rejected_before_loading_secrets() {
    for (section, field) in [
        ("ingest", "mirror_concurrency"),
        ("ingest", "room_concurrency"),
        ("classify", "concurrency"),
        ("ingest", "raw_retention_months"),
        ("extract", "segment_msgs"),
    ] {
        let mut value: toml::Value = toml::from_str(include_str!("../../config.toml")).unwrap();
        value[section][field] = toml::Value::Integer(0);
        let dir = crate::testutil::fresh_root("config", field);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), toml::to_string(&value).unwrap()).unwrap();
        let error = std::panic::catch_unwind(|| load_from_dir(&dir))
            .err()
            .unwrap();
        let message = error.downcast_ref::<String>().map_or("", String::as_str);
        assert!(
            message.contains(field),
            "应先指出非法配置 {field}：{message}"
        );
        assert!(!message.contains("secrets.toml"), "不应等到读取密钥才报错");
    }
}

/// 两条**跨节**约束各自的失效形态 —— serde 一条都查不到。
///
/// 它们防的都是「配置看起来完全正常、进程照常起来、坏事发生在几小时后」：
///   * 池子小于两队之和 → 群在落库时等不到连接，报「落库失败」，
///     而那时这个群的 token 已经烧完了。
///   * 打标预算 ≥ 抽取预算 → 多半是把 `[llm.extract]` 整段抄过去只改了 model。
///     跑飞时唯一会喊停的就只剩 `timeout_secs`，每趟挂死几回。
///
/// **两条都只在启动期看得见，所以必须在这里钉住** —— 生产上没有任何后续步骤
/// 会再检查一遍。
#[test]
fn cross_section_constraints_are_rejected_at_startup() {
    /// 改一处 config.toml，走一遍加载，返回 panic 文案。
    fn rejected(tag: &str, edit: impl FnOnce(&mut toml::Value)) -> String {
        let mut value: toml::Value = toml::from_str(include_str!("../../config.toml")).unwrap();
        edit(&mut value);
        let dir = crate::testutil::fresh_root("config", tag);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), toml::to_string(&value).unwrap()).unwrap();
        let error = std::panic::catch_unwind(|| load_from_dir(&dir))
            .err()
            .expect("非法配置必须 panic");
        let message = error
            .downcast_ref::<String>()
            .map_or(String::new(), Clone::clone);
        assert!(!message.contains("secrets.toml"), "不应等到读取密钥才报错");
        message
    }

    // 池子小于两队之和：10 + 6 = 16，给 8 必然不够。
    let message = rejected("pool", |v| {
        v["mysql"]["max_connections"] = toml::Value::Integer(8);
    });
    assert!(
        message.contains("max_connections") && message.contains("classify.concurrency"),
        "报错要点出两个消费者都算进去了：{message}"
    );

    // 把 `[llm.extract]` 的输出上限整段抄给打标 —— 现实中最可能发生的那种错法。
    let message = rejected("budget", |v| {
        v["llm"]["classify"]["max_tokens"] = v["llm"]["extract"]["max_tokens"].clone();
    });
    assert!(
        message.contains("llm.classify.max_tokens"),
        "报错要点名打标那份预算：{message}"
    );
}

/// 仓库里那份 `config.toml` 必须能填满 [`Config`]。所有键必填、代码里没有默认值，
/// 所以漏一个键就是**进程起不来** —— 让它在 `cargo test` 里炸，别留到跑批那天。
#[test]
fn the_shipped_config_toml_fills_every_field() {
    let text =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
    toml::from_str::<Config>(&text).unwrap();
}

/// 上一条的反面：**缺一个键必须是崩，不是走默认值。**
///
/// 「所有键必填」今天靠的是 serde 在字段缺失时报错，没有任何一处显式检查 ——
/// 也就是说给某个字段加一个 `#[serde(default)]` 是完全无声的：编译过、
/// 上面那条测试照样绿（仓库里那份 config.toml 什么都不缺），只有跑批那天
/// 才发现进程拿着一个谁都没写过的值起来了。这条钉住的就是那个无声改动。
///
/// 拿 `segment_msgs` 开刀是因为它明确「无默认值，缺失即报错」。
#[test]
#[should_panic(expected = "解析失败")]
fn a_missing_key_panics_instead_of_falling_back_to_a_default() {
    let text =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
    let holed: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("segment_msgs"))
        .map(|l| format!("{l}\n"))
        .collect();
    // 注释里也写着 segment_msgs，所以只查赋值行还在不在
    assert!(
        !holed
            .lines()
            .any(|l| l.trim_start().starts_with("segment_msgs")),
        "样本没挖掉那个键，这条测试就白测了"
    );

    let dir = crate::testutil::fresh_root("config", "missing-key");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("config.toml");
    std::fs::write(&p, holed).unwrap();
    let _: Config = load(&p, false);
}

/// **密钥文件的解析报错不许带原文。** `toml` 的错误会回显出错那一行，而唯一
/// 会有人碰 secrets.toml 的场合正是轮换密钥、粘歪一个引号的时候 ——
/// 那一行原样进 stderr 就等于进 run.log / journald / cron 邮件 / CI 输出。
#[test]
fn a_broken_secrets_file_never_echoes_the_key_into_the_error() {
    let dir = crate::testutil::fresh_root("config", "redact");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("secrets.toml");
    // 少一个右引号 —— 粘歪密钥最常见的形态
    std::fs::write(&p, "[llm]\napi_key = \"sk-SUPERSECRET-abc123\n").unwrap();

    let err = std::panic::catch_unwind(|| load::<Secrets>(&p, true))
        .err()
        .unwrap();
    let msg = err
        .downcast_ref::<String>()
        .map_or("", String::as_str)
        .to_string();
    assert!(!msg.contains("SUPERSECRET"), "密钥泄漏进报错了：{msg}");
    assert!(msg.contains("详情已省略"), "{msg}");

    // 反过来钉住 config.toml 那条路仍然打全文 —— 它没有密钥，省掉只会难查
    let c = dir.join("config.toml");
    std::fs::write(&c, "[llm]\nmodel = \"m\n").unwrap();
    let err = std::panic::catch_unwind(|| load::<Config>(&c, false))
        .err()
        .unwrap();
    let msg = err.downcast_ref::<String>().map_or("", String::as_str);
    assert!(msg.contains("TOML parse error"), "{msg}");
}

/// 密钥文件权限过宽必须**拒绝加载**（照 ssh 对私钥的规矩）。
#[cfg(unix)]
#[test]
#[should_panic(expected = "权限过宽")]
fn a_group_readable_secrets_file_is_refused() {
    require_owner_only(&secrets_with_mode("refused", 0o640));
}

/// 上一条的对照组 —— 没有它，「拒绝」也可能只是因为这个函数恒崩。
#[cfg(unix)]
#[test]
fn owner_only_secrets_are_accepted() {
    require_owner_only(&secrets_with_mode("accepted", 0o600));
}

#[cfg(unix)]
fn secrets_with_mode(name: &str, mode: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::testutil::fresh_root("config", name);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("secrets.toml");
    std::fs::write(&p, "").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    p
}
