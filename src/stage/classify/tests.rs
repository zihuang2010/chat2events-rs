//! `classify` 的**跨文件**测试与共享 fixture —— `check` / `model` / `cache` 的私有项
//! 照常可见（子模块的私有项经 `pub(super)`）。
//!
//! **单个文件的单元测试住在各自文件底部**（`model` 的 prompt 与校验、`cache` 的锁与
//! 迁移），fixture 从这里借（`super::super::tests::{ty, known, lab, …}`）。
//! 留在这里的是端到端的 [`Classifier`]：构造校验 · 缓存指纹 · 去重与顺序 · 重试。

use super::{Classifier, Labels, TaxonomyType, UNTYPED, cache::digest, types::Assignment};
use crate::{llm::Llm, rejection::Rejection, testutil::test_classify_llm};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn http_model(
    replies: Vec<Value>,
    wait_for_all: bool,
) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    crate::testutil::http_model(
        replies
            .into_iter()
            .map(|r| (200, crate::testutil::completion(&r.to_string(), "stop")))
            .collect(),
        wait_for_all,
    )
}

fn reply(types: &[&str]) -> Value {
    json!({"assignments":[{"index":1,"type_ids":types}]})
}

#[tokio::test]
async fn rejected_model_evidence_is_only_sent_back_to_the_model() {
    let marker = "PRIVATE-ADDRESS-TEST";
    let (base, server) = http_model(vec![reply(&[marker]), reply(&[marker])], false);
    let root = crate::testutil::fresh_root("classify", "private-rejection");
    let classifier =
        Classifier::new("v1", vec![ty("a")], test_classify_llm(&base, "test"), &root).unwrap();
    let error = classifier.classify(&["商家要求改期"]).await.unwrap_err();
    let requests = server.join().unwrap();
    assert!(
        requests[1]["messages"].as_array().unwrap().iter().any(|m| {
            m["role"] == "user" && m["content"].as_str().is_some_and(|s| s.contains(marker))
        }),
        "模型应收到逐字纠错信息"
    );
    assert!(
        !error.to_string().contains(marker),
        "运维错误不应包含模型逐字证据：{error}"
    );
    assert!(!format!("{error:?}").contains(marker));
}

#[test]
fn constructor_rejects_missing_or_invalid_published_taxonomy() {
    let dir = crate::testutil::fresh_root("classify", "validation");
    let llm = test_classify_llm("http://127.0.0.1:1/v1", "test");
    for (version, types) in [
        ("v1", vec![]),
        ("v0", vec![ty("a")]),
        ("../v1", vec![ty("a")]),
        ("v1", vec![ty("a"), ty("a")]),
        ("v1", vec![ty(UNTYPED)]),
    ] {
        assert!(Classifier::new(version, types, llm.clone(), &dir).is_err());
    }
    let mut invalid = ty("a");
    invalid.description = " ".into();
    assert!(Classifier::new("v1", vec![invalid], llm.clone(), &dir).is_err());
    let mut invalid = ty("a");
    invalid.name = "名称\n伪造规则".into();
    assert!(Classifier::new("v1", vec![invalid], llm, &dir).is_err());
    assert!(!dir.exists(), "词表失败应发生在创建缓存之前");
}

#[test]
fn the_published_taxonomy_passes_the_database_loading_checks() {
    let draft: crate::process::taxonomy::Draft =
        toml::from_str(include_str!("../../../taxonomy_v1.toml")).unwrap();
    let dir = crate::testutil::fresh_root("classify", "published-taxonomy");
    let classifier = Classifier::new(
        &draft.version,
        draft.types.clone(),
        test_classify_llm("http://127.0.0.1:1/v1", "test"),
        &dir,
    )
    .unwrap();
    assert_eq!(classifier.type_count(), draft.types.len());
}

#[tokio::test]
async fn concurrent_model_answers_return_the_same_committed_labels() {
    let (base, server) = http_model(vec![reply(&["a"]), reply(&["b"])], true);
    let dir = crate::testutil::fresh_root("classify", "concurrent-http");
    let llm = test_classify_llm(&base, "test");
    let classifier = Classifier::new("v1", vec![ty("a"), ty("b")], llm.clone(), &dir).unwrap();
    let (left, right) = tokio::join!(
        classifier.classify(&["同一摘要"]),
        classifier.classify(&["同一摘要"])
    );
    let left = left.unwrap();
    assert_eq!(left, right.unwrap());
    assert_eq!(server.join().unwrap().len(), 2);
    drop(classifier);
    // 端点已关闭，重开缓存仍必须返回同一答案。
    let reopened = Classifier::new("v1", vec![ty("a"), ty("b")], llm, &dir).unwrap();
    assert_eq!(reopened.classify(&["同一摘要"]).await.unwrap(), left);
}

#[tokio::test]
async fn saved_answers_cannot_populate_a_different_policy_cache() {
    let dir = crate::testutil::fresh_root("classify", "restore-policy");
    let old = Classifier::new(
        "v1",
        vec![ty("a")],
        test_classify_llm("http://localhost:1/v1", "old"),
        &dir,
    )
    .unwrap();
    old.cache
        .lock()
        .await
        .commit(vec![(digest("已保存摘要"), lab(&["a"]))])
        .unwrap();
    let answers = [("已保存摘要", lab(&["a"]))];
    old.check_saved_answers(&answers).await.unwrap();
    let changed = Classifier::new(
        "v1",
        vec![ty("a")],
        test_classify_llm("http://localhost:1/v1", "new"),
        &dir,
    )
    .unwrap();
    assert!(changed.check_saved_answers(&answers).await.is_err());
    assert_eq!(
        changed.cache.lock().await.len(),
        0,
        "旧模型答案不能污染新策略缓存"
    );
}

#[tokio::test]
async fn model_batches_deduplicate_and_keep_input_order() {
    let first = json!({"assignments":(1..=50).rev().map(|index| json!({"index":index,"type_ids":["a"]})).collect::<Vec<_>>()});
    let (base, server) = http_model(vec![first, reply(&["b"])], false);
    let dir = crate::testutil::fresh_root("classify", "batch-http");
    let classifier = Classifier::new(
        "v1",
        vec![ty("a"), ty("b")],
        test_classify_llm(&base, "test"),
        &dir,
    )
    .unwrap();
    classifier
        .cache
        .lock()
        .await
        .commit(vec![(digest("已缓存摘要"), lab(&["b"]))])
        .unwrap();
    let mut summaries: Vec<_> = (0..51).map(|i| format!("摘要{i}")).collect();
    summaries.extend([
        summaries[0].clone(),
        "已缓存摘要".into(),
        "已缓存摘要".into(),
    ]);
    let got = classifier
        .classify(&summaries.iter().map(String::as_str).collect::<Vec<_>>())
        .await
        .unwrap();
    assert_eq!(got.len(), 54);
    assert!(got[..50].iter().all(|label| label.primary() == "a"));
    assert_eq!(got[50].primary(), "b");
    assert_eq!(got[51].primary(), "a");
    assert!(got[52..].iter().all(|label| label.primary() == "b"));
    let requests = server.join().unwrap();
    // **打标请求必须带着 `[llm.classify]` 那份小预算出门** —— 这是全仓唯一一处
    // 运行时证明它的地方。此前钉的是 `CLASSIFY_MAX_TOKENS` 那个常量（代码里的
    // 无条件钳位），现在真相在 config.toml，所以钉**关系**不钉字面量：
    // 6000 调成 5000 不该让这条误报，而抄成抽取那个 64000 必须让它红。
    let cfg: crate::config::Config = toml::from_str(include_str!("../../../config.toml")).unwrap();
    assert_eq!(requests[0]["max_tokens"], cfg.llm.classify.max_tokens);
    assert!(
        cfg.llm.classify.max_tokens < cfg.llm.extract.max_tokens,
        "打标预算必须远小于抽取预算，否则跑飞时只剩 timeout_secs 喊停"
    );
    assert!(
        requests[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("a | a：描述")
    );
    assert_eq!(
        requests[0]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .lines()
            .count(),
        50
    );
    assert_eq!(requests[1]["messages"][1]["content"], "#1 摘要50\n");
}

#[tokio::test]
async fn invalid_model_output_is_retried_and_never_cached_as_untyped() {
    let (base, server) = http_model(vec![reply(&["unknown"]), reply(&["a"])], false);
    let dir = crate::testutil::fresh_root("classify", "retry-http");
    let classifier =
        Classifier::new("v1", vec![ty("a")], test_classify_llm(&base, "test"), &dir).unwrap();
    assert_eq!(classifier.classify(&["摘要"]).await.unwrap(), [lab(&["a"])]);
    let requests = server.join().unwrap();
    assert!(
        requests[1]["messages"][3]["content"]
            .as_str()
            .unwrap()
            .contains("不在词表里")
    );

    let (base, server) = http_model(vec![reply(&["unknown"]), reply(&["unknown"])], false);
    let classifier = Classifier::new(
        "v1",
        vec![ty("a")],
        test_classify_llm(&base, "test"),
        &dir.join("failed"),
    )
    .unwrap();
    assert!(classifier.classify(&["摘要"]).await.is_err());
    assert_eq!(classifier.cache.lock().await.len(), 0);
    server.join().unwrap();
}

#[test]
fn different_models_versions_and_taxonomies_use_separate_cache_files() {
    let dir = crate::testutil::fresh_root("classify", "policy");
    for (version, model, types) in [
        ("v1", "model-a", vec![ty("a")]),
        ("v1", "model-b", vec![ty("a")]),
        ("v2", "model-a", vec![ty("a")]),
        ("v1", "model-a", vec![ty("b")]),
    ] {
        drop(
            Classifier::new(
                version,
                types,
                test_classify_llm("http://127.0.0.1:1/v1", model),
                &dir,
            )
            .unwrap(),
        );
    }
    assert_eq!(
        std::fs::read_dir(&dir)
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "sqlite"))
            .count(),
        4
    );
}

pub(super) fn ty(id: &str) -> TaxonomyType {
    TaxonomyType {
        type_id: id.into(),
        parent_name: "一级".into(),
        name: id.into(),
        description: "描述".into(),
    }
}

pub(super) fn ty_under(parent: &str, id: &str) -> TaxonomyType {
    TaxonomyType {
        parent_name: parent.into(),
        ..ty(id)
    }
}

pub(super) fn known() -> BTreeSet<String> {
    [ty("a").type_id, ty("b").type_id, UNTYPED.to_string()]
        .into_iter()
        .collect()
}

/// `(行号, 该行的 type_ids)`
pub(super) fn asg(pairs: &[(u32, &[&str])]) -> Vec<Assignment> {
    pairs
        .iter()
        .map(|(i, ts)| Assignment {
            index: *i,
            type_ids: ts.iter().map(|t| (*t).to_string()).collect(),
        })
        .collect()
}

/// 校验结果拍平成 `Vec<Vec<&str>>`，好和字面量直接比。
pub(super) fn flat(r: Result<Vec<Labels>, Rejection>) -> Vec<Vec<String>> {
    r.unwrap().into_iter().map(|l| l.0).collect()
}

pub(super) fn lab(ts: &[&str]) -> Labels {
    Labels(ts.iter().map(|t| (*t).to_string()).collect())
}

/// 缓存文件名**不许依赖词表传进来的顺序**。
///
/// 从库里读的走 `ORDER BY type_id`，从草稿来的走 TOML 里的原序 —— 两边不一致时
/// 试打和正式 recompute 会各写各的缓存文件，「试打不是额外开销」那句话就是假的。
/// 一个请求都不发，纯 CPU。
#[test]
fn the_cache_path_does_not_depend_on_the_order_the_taxonomy_came_in() {
    // 一个请求都不发，Llm 只是构造指纹时的一个输入 —— 拿现成的 fixture 就够。
    let llm = test_classify_llm("http://localhost:1/v1", "m");
    let dir = crate::testutil::fresh_root("classify", "order");
    let path = |types: Vec<TaxonomyType>| {
        drop(Classifier::new("v1", types, llm.clone(), &dir).unwrap());
        std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<BTreeSet<_>>()
    };
    // **跨一级乱序**：只按 type_id 排的话 `b1` 会插到 `a1`/`a2` 中间，
    // 分组标题就会多出来一个，指纹随输入顺序变。
    let x = || {
        vec![
            ty_under("甲", "a1"),
            ty_under("乙", "b1"),
            ty_under("甲", "a2"),
        ]
    };
    let mut shuffled = x();
    shuffled.reverse();
    assert_eq!(path(x()), path(shuffled));
}

/// 显式 v0 且词表为空：全 `__untyped__`，一个请求都不发。
#[tokio::test]
async fn an_empty_taxonomy_is_v0_and_asks_nothing() {
    let dir = crate::testutil::fresh_root("classify", "v0");
    let cfg: crate::config::Config = toml::from_str(
        &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap(),
    )
    .unwrap();
    let c = Classifier::new(
        "v0",
        vec![],
        Llm::new(&cfg.llm, &cfg.llm.classify, "sk-none".into()).unwrap(),
        &dir,
    )
    .unwrap();
    assert_eq!(c.type_count(), 0);
    assert_eq!(
        c.classify(&["甲", "乙"]).await.unwrap(),
        [lab(&[UNTYPED]), lab(&[UNTYPED])]
    );
}
