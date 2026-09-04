//! **BGE-small-zh-v1.5 + 线性分类头** —— 用 `summary` 预测 `event_type` 的离线验证。
//!
//! ```text
//! summary（= intent_text）→ BGE-small-zh-v1.5 → 512 维句向量 → Linear(512→C) → softmax
//! ```
//!
//! ## 隔离
//!
//! **独立 crate，父仓库看不见它。** `chat2events-rs/Cargo.toml` 里没有 `[workspace]`，
//! 所以 `cargo build` / `cargo test` / CI 一个字节都不碰这里；它也不 `use` 任何
//! 业务模块，只读 `b_merchant_group_event` 的两列，**不写任何表**。
//!
//! ## 为什么是 candle，不是 fastembed / ONNX
//!
//! 目标机 CentOS 7（glibc 2.17），ONNX Runtime 自 1.16 起放弃 glibc < 2.28。
//! candle 的 CPU 后端是纯 Rust（`gemm`），tokenizers 关掉了 `onig` / `esaxx_fast`
//! 这两个 C/C++ 特性 —— 整条链没有 native 依赖，编得进目标机。
//!
//! ## 跑之前
//!
//! 模型权重不进 git（96 MB），自己拉：
//!
//! ```sh
//! mkdir -p models/bge-small-zh-v1.5 && cd $_
//! for f in config.json tokenizer.json model.safetensors; do
//!   curl -sLO "https://huggingface.co/BAAI/bge-small-zh-v1.5/resolve/main/$f"
//! done
//! ```
//!
//! 然后 `cargo run --release`。MySQL 地址默认从 `../../secrets.toml` 的 `[mysql] url`
//! 读，也可以用环境变量 `MYSQL_URL` 覆盖；模型目录用 `BGE_DIR` 覆盖。

use anyhow::{Context, Result, anyhow, ensure};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::{Linear, Module, Optimizer, VarBuilder, VarMap, loss};
use candle_transformers::models::bert::{BertModel, Config};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokenizers::Tokenizer;

/// 留出集比例的倒数：每类内部下标 % HOLDOUT == 0 的那些进留出集（≈20%）。
/// 只有一条样本的类整类进训练集 —— 否则它训练集为空，那个类根本没被学过。
const HOLDOUT: usize = 5;
const EPOCHS: usize = 800;
const LR: f64 = 5e-3;
/// 洗牌种子。定死是为了同一份数据重跑结论一致（分类头是零初始化，本身就确定）。
const SEED: u64 = 42;

#[tokio::main]
async fn main() -> Result<()> {
    let model_dir =
        PathBuf::from(std::env::var("BGE_DIR").unwrap_or_else(|_| "models/bge-small-zh-v1.5".into()));

    // ── 数据 ────────────────────────────────────────────────────────────────
    let raw = load_rows().await?;
    println!("表里取到 {} 行", raw.len());

    // **按 summary 去重**：同一句原文同时落进训练集和留出集，测出来的准确率是
    // 背下来的，不是学出来的。1007 行里有 870 句不同的话，不去重虚高一大截。
    let mut by_text: BTreeMap<String, String> = BTreeMap::new();
    let mut conflicts = 0usize;
    for (summary, event_type) in raw {
        if let Some(prev) = by_text.insert(summary, event_type.clone())
            && prev != event_type
        {
            conflicts += 1;
        }
    }
    if conflicts > 0 {
        println!("⚠️  {conflicts} 句 summary 在表里有不止一个 event_type，取字典序最后那个");
    }

    let mut data: Vec<(String, String)> = by_text.into_iter().collect();
    shuffle(&mut data, SEED);

    let mut labels: Vec<String> = data.iter().map(|(_, t)| t.clone()).collect();
    labels.sort();
    labels.dedup();
    let ys: Vec<u32> = data
        .iter()
        .map(|(_, t)| labels.iter().position(|l| l == t).unwrap() as u32)
        .collect();
    println!("去重后 {} 条 / {} 个类", data.len(), labels.len());

    // ── 编码 ────────────────────────────────────────────────────────────────
    let device = Device::Cpu;
    let embedder = Embedder::load(&model_dir, &device)?;
    let t0 = std::time::Instant::now();
    let mut feats: Vec<f32> = Vec::with_capacity(data.len() * 512);
    for (i, (summary, _)) in data.iter().enumerate() {
        feats.extend(embedder.embed(summary)?);
        if (i + 1) % 200 == 0 {
            println!("  编码 {}/{}", i + 1, data.len());
        }
    }
    let dim = feats.len() / data.len();
    println!(
        "编码完成：{} 条 × {dim} 维，{:.1}s（{:.1} 条/s）",
        data.len(),
        t0.elapsed().as_secs_f32(),
        data.len() as f32 / t0.elapsed().as_secs_f32()
    );

    // ── 分层留出 ────────────────────────────────────────────────────────────
    let mut per_class: Vec<Vec<usize>> = vec![Vec::new(); labels.len()];
    for (i, y) in ys.iter().enumerate() {
        per_class[*y as usize].push(i);
    }
    let (mut train, mut test) = (Vec::new(), Vec::new());
    for idxs in &per_class {
        for (k, &i) in idxs.iter().enumerate() {
            if idxs.len() > 1 && k % HOLDOUT == 0 {
                test.push(i);
            } else {
                train.push(i);
            }
        }
    }
    println!("训练 {} 条 / 留出 {} 条", train.len(), test.len());

    let (x_tr, y_tr) = gather(&feats, &ys, &train, dim, &device)?;
    let (x_te, y_te) = gather(&feats, &ys, &test, dim, &device)?;

    // ── 训练线性头 ──────────────────────────────────────────────────────────
    // **零初始化**，不用 candle 默认的 kaiming 随机初始化：`Device::Cpu` 不支持
    // `set_seed`，随机初始化就意味着每次跑出来的数不一样。单层 softmax 回归没有
    // 对称性问题（每个类的梯度本来就不同），零初始化是标准做法。
    let varmap = VarMap::new();
    let c = labels.len();
    let w = varmap.get((c, dim), "head.weight", candle_nn::init::ZERO, DType::F32, &device)?;
    let b = varmap.get(c, "head.bias", candle_nn::init::ZERO, DType::F32, &device)?;
    let head = Linear::new(w, Some(b));
    let mut opt = candle_nn::AdamW::new(
        varmap.all_vars(),
        candle_nn::ParamsAdamW {
            lr: LR,
            ..Default::default()
        },
    )?;

    println!("\nepoch  train_loss  train_acc  test_acc");
    for ep in 1..=EPOCHS {
        let logits = head.forward(&x_tr)?;
        let l = loss::cross_entropy(&logits, &y_tr)?;
        opt.backward_step(&l)?;
        if ep % 100 == 0 || ep == 1 {
            let tr_acc = accuracy(&head.forward(&x_tr)?, &y_tr)?;
            let te_acc = accuracy(&head.forward(&x_te)?, &y_te)?;
            println!(
                "{ep:5}  {:10.4}  {:8.1}%  {:7.1}%",
                l.to_scalar::<f32>()?,
                tr_acc * 100.0,
                te_acc * 100.0
            );
        }
    }

    // ── 结果 ────────────────────────────────────────────────────────────────
    let logits = head.forward(&x_te)?.to_vec2::<f32>()?;
    let truth: Vec<u32> = test.iter().map(|&i| ys[i]).collect();
    report(&labels, &truth, &logits, &train, &ys);

    println!("\n留出集里判错的前 20 条：");
    let mut wrong = 0;
    for (k, &i) in test.iter().enumerate() {
        let pred = argmax(&logits[k]);
        if pred as u32 != ys[i] {
            wrong += 1;
            if wrong <= 20 {
                println!(
                    "  真:{:<24} 判:{:<24} {}",
                    labels[ys[i] as usize], labels[pred], data[i].0
                );
            }
        }
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// 编码器
// ────────────────────────────────────────────────────────────────────────────

struct Embedder {
    bert: BertModel,
    tok: Tokenizer,
    device: Device,
}

impl Embedder {
    fn load(dir: &Path, device: &Device) -> Result<Self> {
        let cfg_path = dir.join("config.json");
        let cfg: Config = serde_json::from_slice(
            &std::fs::read(&cfg_path)
                .with_context(|| format!("读不到 {} —— 先按文件头注释拉模型", cfg_path.display()))?,
        )?;
        // 512 维是这个实验的前提；换成 base（768）说明拉错了模型。
        ensure!(cfg.hidden_size == 512, "hidden_size = {}，不是 512", cfg.hidden_size);
        let tok = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| anyhow!("{e}"))?;
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DType::F32, device)?
        };
        Ok(Self {
            bert: BertModel::load(vb, &cfg)?,
            tok,
            device: device.clone(),
        })
    }

    /// 一条一句，不做 batch —— 870 条几十秒就完了，padding + attention mask 那套
    /// 代码在这个体量上买不到任何东西。
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let enc = self.tok.encode(text, true).map_err(|e| anyhow!("{e}"))?;
        let ids = enc.get_ids();
        let ids = &ids[..ids.len().min(512)]; // max_position_embeddings
        let input = Tensor::new(ids, &self.device)?.unsqueeze(0)?;
        let type_ids = input.zeros_like()?;
        let out = self.bert.forward(&input, &type_ids, None)?; // [1, L, 512]
        // BGE 系列用 **CLS 池化 + L2 归一化**，不是均值池化 —— 换成均值分数会掉。
        let cls = out.i((0, 0))?;
        let norm = cls.sqr()?.sum_all()?.sqrt()?.to_scalar::<f32>()?;
        Ok(cls.affine(1.0 / norm as f64, 0.0)?.to_vec1::<f32>()?)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 数据
// ────────────────────────────────────────────────────────────────────────────

async fn load_rows() -> Result<Vec<(String, String)>> {
    let url = match std::env::var("MYSQL_URL") {
        Ok(u) => u,
        Err(_) => {
            let raw = std::fs::read_to_string("../../secrets.toml")
                .context("既没有 MYSQL_URL，也读不到 ../../secrets.toml")?;
            raw.lines()
                .skip_while(|l| l.trim() != "[mysql]")
                .find_map(|l| l.trim().strip_prefix("url")?.split('"').nth(1))
                .context("secrets.toml 的 [mysql] 段里没有 url")?
                .to_string()
        }
    };
    let pool = sqlx::MySqlPool::connect(&url).await?;
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT summary, event_type FROM b_merchant_group_event ORDER BY id",
    )
    .fetch_all(&pool)
    .await?;
    ensure!(!rows.is_empty(), "b_merchant_group_event 是空的");
    Ok(rows)
}

fn gather(
    feats: &[f32],
    ys: &[u32],
    idx: &[usize],
    dim: usize,
    dev: &Device,
) -> Result<(Tensor, Tensor)> {
    let mut x = Vec::with_capacity(idx.len() * dim);
    let mut y = Vec::with_capacity(idx.len());
    for &i in idx {
        x.extend_from_slice(&feats[i * dim..(i + 1) * dim]);
        y.push(ys[i]);
    }
    Ok((
        Tensor::from_vec(x, (idx.len(), dim), dev)?,
        Tensor::from_vec(y, idx.len(), dev)?,
    ))
}

/// 定种子的 Fisher-Yates（LCG 取随机数）。不引 rand：只需要「每次一样」。
fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut s = seed;
    for i in (1..v.len()).rev() {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.swap(i, (s >> 33) as usize % (i + 1));
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 评估
// ────────────────────────────────────────────────────────────────────────────

fn argmax(row: &[f32]) -> usize {
    row.iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap()
        .0
}

fn accuracy(logits: &Tensor, y: &Tensor) -> Result<f32> {
    let pred = logits.argmax(1)?;
    let hit = pred.eq(y)?.to_dtype(DType::F32)?.sum_all()?.to_scalar::<f32>()?;
    Ok(hit / y.dims1()? as f32)
}

fn report(labels: &[String], truth: &[u32], logits: &[Vec<f32>], train: &[usize], ys: &[u32]) {
    let c = labels.len();
    let (mut tp, mut fp, mut fal_neg, mut support) = (vec![0; c], vec![0; c], vec![0; c], vec![0; c]);
    let (mut top1, mut top3) = (0usize, 0usize);
    let mut confusion: BTreeMap<(usize, usize), usize> = BTreeMap::new();

    for (row, &t) in logits.iter().zip(truth) {
        let t = t as usize;
        support[t] += 1;
        let p = argmax(row);
        if p == t {
            top1 += 1;
            tp[t] += 1;
        } else {
            fp[p] += 1;
            fal_neg[t] += 1;
            *confusion.entry((t, p)).or_default() += 1;
        }
        // top-3：排序一次拿前三，看「正确答案在不在候选里」
        let mut order: Vec<usize> = (0..c).collect();
        order.sort_by(|&a, &b| row[b].total_cmp(&row[a]));
        if order[..3.min(c)].contains(&t) {
            top3 += 1;
        }
    }

    // 多数类基线：训练集里最多的那个类，全都猜它能对多少
    let mut freq = vec![0usize; c];
    for &i in train {
        freq[ys[i] as usize] += 1;
    }
    let major = argmax(&freq.iter().map(|&n| n as f32).collect::<Vec<_>>());
    let base = support[major] as f32 / truth.len() as f32;

    let n = truth.len() as f32;
    println!("\n════ 留出集 {} 条 ════", truth.len());
    println!("多数类基线（全猜 {}）  {:.1}%", labels[major], base * 100.0);
    println!("top-1 准确率            {:.1}%", top1 as f32 / n * 100.0);
    println!("top-3 准确率            {:.1}%", top3 as f32 / n * 100.0);

    let mut f1s = Vec::new();
    println!("\n类别                      支持  精确率  召回率      F1");
    let mut order: Vec<usize> = (0..c).filter(|&i| support[i] > 0).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(support[i]));
    for i in order {
        let p = if tp[i] + fp[i] > 0 {
            tp[i] as f32 / (tp[i] + fp[i]) as f32
        } else {
            0.0
        };
        let r = tp[i] as f32 / support[i] as f32;
        let f1 = if p + r > 0.0 { 2.0 * p * r / (p + r) } else { 0.0 };
        f1s.push(f1);
        println!(
            "{:<24} {:5}  {:5.0}%  {:5.0}%  {:6.2}",
            labels[i],
            support[i],
            p * 100.0,
            r * 100.0,
            f1
        );
    }
    println!(
        "\n宏平均 F1（只算留出集里出现过的 {} 个类）  {:.3}",
        f1s.len(),
        f1s.iter().sum::<f32>() / f1s.len() as f32
    );
    let missing = (0..c).filter(|&i| support[i] == 0).count();
    if missing > 0 {
        println!("⚠️  另有 {missing} 个类在留出集里一条样本都没有，这张表说不了它们的话");
    }

    let mut conf: Vec<_> = confusion.into_iter().collect();
    conf.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    println!("\n混淆最多的前 10 对（真 → 判）：");
    for ((t, p), n) in conf.into_iter().take(10) {
        println!("  {n:3}  {} → {}", labels[t], labels[p]);
    }
}
