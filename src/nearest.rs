//! **最近类心分类器** —— 字符 bigram + IDF，纯本地、零依赖、确定性。
//!
//! 只服务 `recompute`（人工触发的重打标）。`daily` 和 `classify` 一字未动 ——
//! 那条路每天只打新增的几千条，问模型是最好的答案。
//!
//! ## 为什么是它，不是 embedding
//!
//! 实测（三份真实样本 466 / 724 / 1932 条 summary）：**去重只省 8~21%**，
//! 抹掉数字后再去重**一条都不多收**。所以 25 万事件就是 25 万次真实打标，
//! 按单流 ~110 tok/s 算是 11.4 流·小时 —— 光靠并发压不进 1 小时的预算。
//!
//! 而当初反对整句 embedding 的那条实测理由（「51.7% 以『商家要求』开头，
//! 相似度被句式主导」）**恰好是 IDF 的主场**：高频句式文档频率高、IDF 低，
//! 自动被压掉，权重落到真正的区分信号上 —— 动词（要求/反馈/催促）和
//! 宾语（改地址/转师傅/取消）。
//!
//! fastembed / ONNX 这条路另评过：目标机是 CentOS 7（glibc 2.17），而
//! ONNX Runtime 自 1.16 起放弃 glibc < 2.28，CI 的 objdump 断言会当场拦下。
//! 这里零依赖，那个问题根本不存在。
//!
//! ## 确定性
//!
//! 训练集来自 `store::read_summary_counts`（`GROUP BY summary ORDER BY n DESC, summary`，
//! 顺序确定），种子标签来自 `Classifier`（第一轮之后全部命中缓存）。
//! 所以 **同一版词表重跑，类心逐比特相同、预测逐条相同** —— 承重不变量 1 要的
//! 「修正而非抖动」在这条路上照样成立。
//!
//! ⚠️ **这一点是靠「所有向量按 term id 排好序」撑住的，不是自动成立的。**
//! 浮点加法不满足结合律，而 `HashMap` 的迭代序每个实例都不一样 ——
//! 直接对着 `HashMap` 求 L2 范数，两次训练的 margin 就会在末位不同，
//! 卡在阈值边上的那些摘要会随机换标签。`training_is_deterministic`
//! 那条测试钉的就是这件事，别为了「省一次 sort」把它拆掉。
//!
//! ## 它绝不做的一件事
//!
//! **差距不够时回落去问模型，绝不硬塞一个最近的类。** 承重不变量 4：
//! 「没算出来」不许表现成一个正常取值。`__untyped__` 只能是模型给的答案。

use crate::{BoxError, classify::Labels};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    path::Path,
};

/// 一个标签组合至少要有几个种子才配有类心。
///
/// 1~2 个种子的类心就是那几条样本本身，余弦会虚高 —— 与其给一个自信的错答案，
/// 不如根本没有这个类心、让它落回模型。**这不是旋钮**：没有第二个取值的需求。
const MIN_SEEDS: usize = 3;

/// 一次预测的结果。
///
/// **能不能信看 `margin`，不看余弦本身** —— 余弦 0.9 但两个类咬到只差 0.005，
/// 说明这条摘要正落在边界上，恰恰是该交给模型的那种。所以这里只带 `margin`。
pub struct Hit<'a> {
    pub labels: &'a Labels,
    /// 最近类心与次近类心的余弦之差。
    pub margin: f32,
}

pub struct Nearest {
    /// 下标即 class id。**按整份 `Labels` 分类，不按主类** —— 跟
    /// `store::retag_room` 的分组同一个理由：主类相同副类不同是两回事。
    classes: Vec<Labels>,
    /// bigram -> term id。预测时没见过的 bigram 直接跳过（标准做法）。
    vocab: HashMap<u64, u32>,
    idf: Vec<f32>,
    /// 每个类的类心，按 term id 升序。落盘的就是它。
    centroids: Vec<Vec<(u32, f32)>>,
    /// term id -> [(class id, 类心在该维的权重)]。**倒排**：查询只有几十个 bigram，
    /// 按词遍历比对每个类做一遍稀疏点积快两个数量级。加载时由 `centroids` 重建，
    /// 不落盘 —— 它是纯派生数据。
    postings: Vec<Vec<(u32, f32)>>,
}

/// 落盘形态。`vocab` 存**排好序的键值对**而不是 map：JSON 对象的键序不保证，
/// 而这个模型的全部确定性都建立在顺序上。
#[derive(Serialize, Deserialize)]
struct Model {
    classes: Vec<Vec<String>>,
    vocab: Vec<(u64, u32)>,
    idf: Vec<f32>,
    centroids: Vec<Vec<(u32, f32)>>,
}

impl Nearest {
    /// 从 (摘要, 标签) 种子训练。种子标签由 `Classifier` 给 —— 这就是「LLM 教一次」。
    pub fn train(seeds: &[(String, Labels)]) -> Self {
        // 1. 按整份 Labels 分组，够 MIN_SEEDS 的才立一个类。
        let mut by_label: BTreeMap<&Labels, Vec<&str>> = BTreeMap::new();
        for (s, l) in seeds {
            by_label.entry(l).or_default().push(s);
        }
        let kept: Vec<(&Labels, Vec<&str>)> = by_label
            .into_iter()
            .filter(|(_, v)| v.len() >= MIN_SEEDS)
            .collect();

        // 2. 词表 + 文档频率。语料是**被留下的那些种子**，不是全部 ——
        //    IDF 要跟类心算在同一批文档上，否则权重和向量对不齐。
        let docs: Vec<&str> = kept.iter().flat_map(|(_, v)| v.iter().copied()).collect();
        let mut vocab: HashMap<u64, u32> = HashMap::new();
        let mut df: Vec<u32> = Vec::new();
        let mut buf = Vec::new();
        for d in &docs {
            grams(d, &mut buf);
            buf.sort_unstable();
            buf.dedup(); // df 数的是「有几篇文档出现过」，不是出现几次
            for g in &buf {
                let next = vocab.len() as u32;
                match vocab.entry(*g) {
                    std::collections::hash_map::Entry::Occupied(e) => df[*e.get() as usize] += 1,
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(next);
                        df.push(1);
                    }
                }
            }
        }
        let n = docs.len() as f32;
        let idf: Vec<f32> = df.iter().map(|d| (n / *d as f32).ln()).collect();

        // 3. 类心 = 该类全部种子的 L2 归一化 tf-idf 向量之和，再归一化。
        //    先归一化每条再求和，长摘要才不会因为词多就主导类心。
        let mut classes = Vec::with_capacity(kept.len());
        let mut centroids: Vec<Vec<(u32, f32)>> = Vec::with_capacity(kept.len());
        for (label, sums) in &kept {
            let mut acc: HashMap<u32, f32> = HashMap::new();
            for s in sums {
                let mut v = tfidf(s, &vocab, &idf, &mut buf);
                normalize(&mut v); // 先归一化每条再求和 —— 否则长摘要因为词多就主导类心
                for (t, w) in v {
                    *acc.entry(t).or_default() += w;
                }
            }
            // 排序后再归一化：范数是对整份向量求和，`HashMap` 的迭代序会让它不确定。
            let mut cen: Vec<(u32, f32)> = acc.into_iter().collect();
            cen.sort_unstable_by_key(|(t, _)| *t);
            normalize(&mut cen);
            classes.push((*label).clone());
            centroids.push(cen);
        }

        // 4. 倒排。
        let mut postings: Vec<Vec<(u32, f32)>> = vec![Vec::new(); vocab.len()];
        for (c, cen) in centroids.iter().enumerate() {
            for (t, w) in cen {
                postings[*t as usize].push((c as u32, *w));
            }
        }
        // 倒排表内部顺序不影响结果：每个 `scores[c]` 的累加序由查询向量的顺序决定，
        // 而那个是排好序的。
        Self {
            classes,
            vocab,
            idf,
            centroids,
            postings,
        }
    }

    /// 写模型。由 `recompute` 在训练完之后调用 —— **那是唯一「教一次」的地方**。
    pub fn save(&self, path: &Path) -> Result<(), BoxError> {
        let mut vocab: Vec<(u64, u32)> = self.vocab.iter().map(|(g, t)| (*g, *t)).collect();
        vocab.sort_unstable();
        let m = Model {
            classes: self.classes.iter().map(|l| l.all().to_vec()).collect(),
            vocab,
            idf: self.idf.clone(),
            centroids: self.centroids.clone(),
        };
        // ⚠️ **这个文件带 PII，跟同目录的打标缓存不是一个密级。**
        // 缓存那份只有 `sha256(summary)`（`classify::Cache` 顶上论证过），而这里的
        // `vocab` 是字符 bigram：`grams` 把两个字拼进一个 u64，数字虽然抹成 `#`，
        // 但**姓名和地址一概不掩**（承重不变量 7），相邻 bigram 首尾相接就能把师傅
        // 姓名这类片段读回来。换无密钥哈希救不了它 —— bigram 的取值域只有几亿，
        // 整域穷举建反查表是几分钟的事；真要单向就得引密钥管理，而这个文件本来就
        // 躺在 36 GB 明文 raw 区旁边，那道门不值得单独修。
        // **所以按「跟 raw 区同级」处理：只有属主能读。**
        if let Some(d) = path.parent() {
            let mut b = std::fs::DirBuilder::new();
            b.recursive(true);
            #[cfg(unix)]
            b.mode(0o700);
            b.create(d)?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        opts.mode(0o600);
        opts.open(path)?.write_all(&serde_json::to_vec(&m)?)?;
        Ok(())
    }

    /// 读模型。**文件不存在 = `Ok(None)`**，不是错误 —— 那是「还没训练过」的
    /// 正常状态（v0、或者第一次 recompute 之前），调用方退回全部问模型。
    /// 文件存在但坏了才是错误：静默当没有会让跑批悄悄多烧几万次调用。
    pub fn load(path: &Path) -> Result<Option<Self>, BoxError> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let m: Model = serde_json::from_slice(&bytes)?;
        let classes: Vec<Labels> = m
            .classes
            .into_iter()
            .map(|v| {
                Labels::from_saved(v).ok_or_else(|| {
                    BoxError::from(format!("类心模型 {} 里有空标签", path.display()))
                })
            })
            .collect::<Result<_, _>>()?;
        if classes.len() != m.centroids.len() {
            return Err(format!("类心模型 {} 的类数和类心数对不上", path.display()).into());
        }
        let mut postings: Vec<Vec<(u32, f32)>> = vec![Vec::new(); m.vocab.len()];
        for (c, cen) in m.centroids.iter().enumerate() {
            for (t, w) in cen {
                let slot = postings
                    .get_mut(*t as usize)
                    .ok_or_else(|| BoxError::from("类心模型里的 term id 越出词表"))?;
                slot.push((c as u32, *w));
            }
        }
        Ok(Some(Self {
            classes,
            vocab: m.vocab.into_iter().collect(),
            idf: m.idf,
            centroids: m.centroids,
            postings,
        }))
    }

    /// 有几个类心。种子太少 / 太碎时这个数会明显小于词表规模，那是信号。
    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    /// 贴一条。`None` = 一个已知 bigram 都没有，或根本没有类心 —— 交给模型。
    pub fn predict(&self, summary: &str) -> Option<Hit<'_>> {
        let mut buf = Vec::new();
        let mut q = tfidf(summary, &self.vocab, &self.idf, &mut buf);
        if q.is_empty() {
            return None; // 一个已知 bigram 都没有 —— 交给模型
        }
        normalize(&mut q);
        // 类心已归一化，所以点积就是余弦。
        let mut scores = vec![0.0f32; self.classes.len()];
        for (t, w) in &q {
            for (c, cw) in &self.postings[*t as usize] {
                scores[*c as usize] += w * cw;
            }
        }
        // 取前二。类数是几十到几百，一遍线性扫比排序便宜。
        let (mut i1, mut s1, mut s2) = (usize::MAX, f32::MIN, f32::MIN);
        for (i, s) in scores.iter().enumerate() {
            if *s > s1 {
                s2 = s1;
                s1 = *s;
                i1 = i;
            } else if *s > s2 {
                s2 = *s;
            }
        }
        if i1 == usize::MAX {
            return None;
        }
        Some(Hit {
            labels: &self.classes[i1],
            margin: s1 - if s2 == f32::MIN { 0.0 } else { s2 },
        })
    }
}

/// 字符 bigram。**数字统一成 `#`** —— 单号 / 数量 / 电话是纯噪声，
/// 实测抹掉它们对去重没有帮助，但对 IDF 有：不抹的话每个单号都是一个
/// 只出现一次的 bigram，白占词表还把向量拉稀。
fn grams(s: &str, out: &mut Vec<u64>) {
    out.clear();
    let cs: Vec<char> = s
        .chars()
        .map(|c| if c.is_ascii_digit() { '#' } else { c })
        .collect();
    for w in cs.windows(2) {
        out.push((w[0] as u64) << 32 | w[1] as u64);
    }
}

/// 一条文本的 tf-idf。**词表外的 bigram 直接跳过** —— 预测时见到新词是常态，
/// 它没有 IDF 可用，猜一个等于编造。
fn tfidf(s: &str, vocab: &HashMap<u64, u32>, idf: &[f32], buf: &mut Vec<u64>) -> Vec<(u32, f32)> {
    grams(s, buf);
    let mut tf: HashMap<u32, f32> = HashMap::new();
    for g in buf.iter() {
        if let Some(t) = vocab.get(g) {
            *tf.entry(*t).or_default() += 1.0;
        }
    }
    let mut v: Vec<(u32, f32)> = tf
        .into_iter()
        .map(|(t, c)| (t, c * idf[t as usize]))
        .collect();
    // **必须排序**：下游的 L2 归一化和类心累加都是浮点求和，顺序变了结果就变。
    v.sort_unstable_by_key(|(t, _)| *t);
    v
}

/// 就地 L2 归一化。全零向量原样返回 —— 调用方负责把它当「贴不了」处理。
fn normalize(v: &mut [(u32, f32)]) {
    let n = v.iter().map(|(_, w)| w * w).sum::<f32>().sqrt();
    if n > 0.0 {
        for (_, w) in v.iter_mut() {
            *w /= n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本模块只把 `Labels` 当不透明的键和返回值用，不看内容 ——
    /// 所以测试造几个假的就够，不必真发请求。
    fn seeds() -> Vec<(String, Labels)> {
        // 两个明显不同的类，各 4 条；用真实样本里的句式。
        let a: Vec<&str> = vec![
            "商家催促订单安装，平台已通知师傅联系客户预约。",
            "商家催促另一订单安装，平台已通知师傅联系客户预约。",
            "商家催促订单安装，要求明天上门；平台回复稍等后确认该单已约好30号安装。",
            "商家催促两个订单师傅接单，平台表示正在加速调度中。",
        ];
        let b: Vec<&str> = vec![
            "商家要求为订单更换能正常上门安装的师傅。",
            "商家要求为另一订单更换能正常上门安装的师傅，平台表示将协调区域换人。",
            "商家要求更换对接师傅，平台核实后表示会换人。",
            "商家要求换一个师傅上门，平台已安排换人。",
        ];
        let (la, lb) = (Labels::for_test("催单"), Labels::for_test("换人"));
        a.into_iter()
            .map(|s| (s.to_string(), la.clone()))
            .chain(b.into_iter().map(|s| (s.to_string(), lb.clone())))
            .collect()
    }

    #[test]
    fn centroids_separate_the_two_obvious_classes() {
        let n = Nearest::train(&seeds());
        assert_eq!(n.class_count(), 2, "两个类各 4 条种子，都该立起类心");

        let hit = n.predict("商家催促订单安装，平台已通知师傅。").unwrap();
        assert_eq!(hit.labels.primary(), "催单");
        assert!(hit.margin > 0.0, "同类应该明显领先，margin={}", hit.margin);

        let hit = n.predict("商家要求换个师傅来装。").unwrap();
        assert_eq!(hit.labels.primary(), "换人");
    }

    #[test]
    fn a_class_with_too_few_seeds_gets_no_centroid() {
        let mut s = seeds();
        s.push(("商家询问发票开具流程。".into(), Labels::for_test("发票")));
        let n = Nearest::train(&s);
        assert_eq!(n.class_count(), 2, "只有 1 条种子的类不该立类心");
    }

    #[test]
    fn nothing_known_returns_none_instead_of_a_confident_guess() {
        let n = Nearest::train(&seeds());
        // 一个 bigram 都不在词表里 —— 必须是 None，交给模型，不许硬塞。
        assert!(n.predict("XYZ").is_none());
    }

    /// 落盘再读回来必须**逐比特一样**。倒排表是从类心重建的派生数据，
    /// 重建错了这里就红 —— 而线上表现只会是「daily 和 recompute 标得不一样」，
    /// 那种错要几周后看报表才发现。
    #[test]
    fn a_saved_model_round_trips() {
        let dir = crate::testutil::fresh_root("nearest", "model");
        let path = dir.join("v1-nearest.json");
        let a = Nearest::train(&seeds());
        a.save(&path).unwrap();
        let b = Nearest::load(&path).unwrap().expect("刚写的文件必须读得到");

        assert_eq!(a.class_count(), b.class_count());
        for s in ["商家催促订单安装。", "商家要求换个师傅来装。", "无关的话"]
        {
            match (a.predict(s), b.predict(s)) {
                (Some(x), Some(y)) => {
                    assert_eq!(x.labels, y.labels, "{s}");
                    assert_eq!(x.margin.to_bits(), y.margin.to_bits(), "{s}");
                }
                (None, None) => {}
                _ => panic!("{s}：一边贴得上一边贴不上"),
            }
        }
    }

    /// 文件不存在是 `Ok(None)`（还没训练过），不是错误 —— v0 和首次重打标之前
    /// 就是这个状态，报错会让跑批起不来。
    #[test]
    fn a_missing_model_is_not_an_error() {
        let dir = crate::testutil::fresh_root("nearest", "missing");
        assert!(Nearest::load(&dir.join("nope.json")).unwrap().is_none());
    }

    #[test]
    fn training_is_deterministic() {
        let (a, b) = (Nearest::train(&seeds()), Nearest::train(&seeds()));
        let h1 = a.predict("商家催促订单安装。").unwrap();
        let h2 = b.predict("商家催促订单安装。").unwrap();
        assert_eq!(h1.labels, h2.labels);
        assert_eq!(
            h1.margin.to_bits(),
            h2.margin.to_bits(),
            "同输入必须逐比特相同"
        );
    }
}
