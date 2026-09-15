//! 独立打标队列：channel 只传群标识与事件数，所有群共享批次名额。

use crate::{
    BoxError,
    stage::classify::{BATCH, Classifier, Label},
    stage::metrics::{self, Attribution},
    stage::store,
    window::Window,
};
use chrono::NaiveDate;
use futures_util::{StreamExt, stream};
use sqlx::MySqlPool;
use std::{collections::BTreeMap, future::Future};
use tokio::sync::{Semaphore, mpsc};

#[derive(Debug)]
pub(super) struct ClassifyTask {
    pub corpid: String,
    pub roomid: String,
    pub event_count: usize,
}

#[derive(Default)]
pub(super) struct ClassifyTally {
    pub ok: usize,
    pub failed: usize,
    /// 到点没轮到的群。**存名单不存计数** —— 这批群还欠一行 `run_failure`，而本文件
    /// 刻意不认识 `store`（同 `run_rooms`），记账只能由调用方做，名单得先出得来。
    ///
    /// 只留一个计数器时，库里对这批群是**整行缺失**：事实在、标签是 NULL、
    /// 而没有任何东西解释为什么 —— 几个月后报表上那个洞查不出原因。
    pub skipped: Vec<(String, String)>,
}

/// 一个群打标这一趟的结局。`Skipped` 带名单出来，见 [`ClassifyTally::skipped`]。
enum Done {
    Ok,
    Failed(BoxError),
    Skipped(String, String),
}

/// 接收端关闭后排空所有任务；失败按群记账，不取消其他群。
///
/// `deadline` 是**整轮**的预算，与 `mirror::sync` 用的是同一个点，语义也一样：
/// **过点不开新批次**，已经在飞的等完（最坏一个 `request_timeout_secs`）。
///
/// ⚠️ **到点之后仍然把 channel 排空**，只是不执行 —— 每个取出来的群进 `skipped`，
/// 由调用方记一行 `run_failure(stage = "classify")`。直接 drop 掉接收端是那种
/// 「库里整行缺失」的错法，⑪ 刚在抽取侧修掉过一次，不在这边再犯。
///
/// ⚠️ **这条 deadline 是打标队列独立出来之后补回来的**：此前打标在 `run_room` 里，
/// 受「过点不开新群」管着；独立成队列时那道保护是无意间掉的，于是分类端点夜里退化
/// 能让进程跑过 24 小时，下一轮在 `classify::Cache` 的文件锁上失败 ——
/// **整轮零事实、零指标、零 `run_failure`**。
pub(super) async fn run_classification<F, Fut>(
    rx: mpsc::Receiver<ClassifyTask>,
    concurrency: usize,
    deadline: std::time::Instant,
    f: F,
) -> ClassifyTally
where
    F: Fn(ClassifyTask) -> Fut,
    Fut: Future<Output = Result<(), BoxError>>,
{
    let tasks = stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|task| (task, rx))
    });
    let results = tasks
        .map(|task| async {
            if std::time::Instant::now() >= deadline {
                return Done::Skipped(task.corpid, task.roomid);
            }
            match f(task).await {
                Ok(()) => Done::Ok,
                Err(error) => Done::Failed(error),
            }
        })
        .buffer_unordered(concurrency);
    tokio::pin!(results);
    let mut tally = ClassifyTally::default();
    while let Some(done) = results.next().await {
        match done {
            Done::Ok => tally.ok += 1,
            Done::Failed(error) => {
                tally.failed += 1;
                tracing::error!("群打标失败：{error}");
            }
            Done::Skipped(corp, room) => tally.skipped.push((corp, room)),
        }
    }
    tally
}

pub(super) async fn classify_room(
    pool: &MySqlPool,
    classifier: &Classifier,
    slots: &Semaphore,
    concurrency: usize,
    run_date: NaiveDate,
    window: &Window,
    task: ClassifyTask,
) -> Result<(), BoxError> {
    let result = label_events(pool, classifier, slots, concurrency, window, &task).await;
    if let Err(error) = &result {
        let reason = format!("打标失败：{error}");
        store::fail_classification(
            pool,
            run_date,
            store::Shard::new(&task.corpid, &task.roomid, window),
            &reason,
        )
        .await
        .map_err(|write_error| {
            format!(
                "{}：{reason}；记录打标失败也失败：{write_error}",
                task.roomid
            )
        })?;
    }
    result.map_err(|error| format!("{}：{error}", task.roomid).into())
}

async fn label_events(
    pool: &MySqlPool,
    classifier: &Classifier,
    slots: &Semaphore,
    concurrency: usize,
    window: &Window,
    task: &ClassifyTask,
) -> Result<(), BoxError> {
    // 「群 × 本次窗口」是这一段全程的作用域，取一次贯穿到底 ——
    // 此前每个 store 调用点各自把 `window` 拆成两个裸日期再传，拆了三遍。
    let shard = store::Shard::new(&task.corpid, &task.roomid, window);
    let (ids, events) = store::read_events(pool, shard).await?;
    if events.len() != task.event_count {
        return Err(format!(
            "已存事件数发生变化：任务 {}，数据库 {}",
            task.event_count,
            events.len()
        )
        .into());
    }
    let mut saved = store::read_event_labels(pool, shard, classifier).await?;
    let mut labels: Vec<Option<Label>> = ids
        .iter()
        .map(|id| {
            saved
                .remove(id)
                .ok_or_else(|| BoxError::from("读取标签期间事件已变化，请避免跑批与补标并行"))
        })
        .collect::<Result<_, _>>()?;
    let mut completed = BTreeMap::new();
    for (event, label) in events.iter().zip(&labels) {
        if let Some(label) = label
            && completed
                .insert(event.summary.as_str(), label.clone())
                .is_some_and(|previous| previous != *label)
        {
            return Err("同一摘要已有不同标签，不能推断恢复答案".into());
        }
    }
    classifier
        .check_saved_answers(&completed.into_iter().collect::<Vec<_>>())
        .await?;
    // 同一摘要只占一个模型位置，回写时保留它对应的全部事件 ID。
    let mut unique: BTreeMap<&str, Vec<(usize, u64)>> = BTreeMap::new();
    for (index, (id, event)) in ids.iter().zip(&events).enumerate() {
        if labels[index].is_some() {
            continue;
        }
        unique.entry(&event.summary).or_default().push((index, *id));
    }
    let work: Vec<_> = unique.into_iter().collect();
    let batches = stream::iter(work.chunks(BATCH))
        .map(|batch| async move {
            let waiting = std::time::Instant::now();
            let _permit = slots
                .acquire()
                .await
                .expect("批次名额仅由打标队列持有，不主动关闭");
            let queue_wait_ms = waiting.elapsed().as_millis();
            let started = std::time::Instant::now();
            let summaries: Vec<_> = batch.iter().map(|(summary, _)| *summary).collect();
            let result = classifier.classify(&summaries).await;
            tracing::info!(corp = %task.corpid, room = %task.roomid, summaries = summaries.len(),
                queue_wait_ms, classify_ms = started.elapsed().as_millis(), ok = result.is_ok(), "分类批次完成");
            let labels = result?;
            let mut ids = Vec::new();
            let mut positions = Vec::new();
            let mut expanded = Vec::new();
            for ((_, entries), label) in batch.iter().zip(labels) {
                for (position, id) in entries {
                    ids.push(*id);
                    positions.push(*position);
                    expanded.push(label.clone());
                }
            }
            store::update_event_labels(pool, shard, &ids, &expanded, classifier.version()).await?;
            Ok::<_, BoxError>((positions, expanded))
        })
        .buffer_unordered(concurrency);
    tokio::pin!(batches);
    let mut failure = None;
    // 某批失败也收完其他批次，成功结果独立保存，绝不提前取消在飞写入。
    while let Some(result) = batches.next().await {
        match result {
            Ok((positions, got)) => {
                for (position, label) in positions.into_iter().zip(got) {
                    labels[position] = Some(label);
                }
            }
            Err(error) => {
                tracing::error!(corp = %task.corpid, room = %task.roomid, "打标批次失败：{error}");
                failure.get_or_insert(error);
            }
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    let types: Vec<_> = labels
        .iter()
        .map(|label| {
            label
                .as_ref()
                .expect("所有批次成功，每个事件均已获得标签")
                .type_id()
        })
        .collect();
    let agent = metrics::agent_rows(
        &task.corpid,
        &task.roomid,
        &events,
        &types,
        classifier.version(),
        Attribution::default(),
    );
    store::finish_classification(pool, shard, &agent).await
}
