//! 正文脱敏 —— **把正文交给模型的唯一出口**（承重不变量 7），全部实现在这里。
//!
//! 论证与实测数字见 **ADR-0001**。三件事在这个文件里，缺一不可：
//!   * 六个正则（订单号 · 引用块 · `@` · 手机号 · 折行 · 结构化工单字段）；
//!   * 手机号两侧的非数字断言 —— Rust 的 `regex` 没有后顾断言，手写在 [`phone_spans`]；
//!   * [`body`] 里那个**顺序**：引用块先删 → `@` → 手机号 → 折行 → 字段。
//!     顺序错了不报错，只是掩不干净、或者把订单号掩掉半截。
//!
//! [`ORDER_NO`] 也住在这里：它和脱敏共用「什么是订单号」这**一个**定义，
//! 而便签、`summary` 校验、孤儿哨兵三处都要问它。

use crate::ingest::Message;
use regex::Regex;
use std::sync::LazyLock;

// ─────────────────────────────────────────────────────────────────────────────
// 正则 —— 全部服务于 `_body` 与便签的订单号线索
// ─────────────────────────────────────────────────────────────────────────────

/// 订单号/工单号。派单群靠它指明这条消息说的是哪一单，实测 1096 条里 358 条带它。
///
/// **不锚行首**：商家习惯把单号写在开头（322 条），可平台的结构化工单推送一律写在
/// 正文中间（「工单原因：… / 订单号:JDLY… 」「三方：5127…」），那 36 条曾经一条都
/// 进不了便签 —— 而便签带订单号正是这个群最可靠的关联线索，平台侧全漏等于线索少一半。
///
/// 16 位下限把手机号（11 位）和短数字挡在外面 —— 手机号是「联系客户用」的，
/// 一个客服名下十几单共用一个，当关联键会把不相干的单焊死。
pub(super) static ORDER_NO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z]{0,6}\d{16,}").unwrap());

/// 上游把**被引用的整条原文连同真实姓名**塞进了 `analysisText`：
/// `"王鸿江：\n<被引用的原文>"\n------\n<回复正文>`
///
/// 1850 条样本里 20 条这种结构，而 `super::render` 已经把那条 `replyTo` 渲染成
/// `↩回复 #N` 了 —— 所以引用块是同一信息的第二份拷贝，且是**带姓名**的那份。
/// Python 的 `re.DOTALL` 在这里是 `(?s)`。
static QUOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)^".+?：\n.*?"\n-+\n"#).unwrap());

/// `@` 后面可能跟一个括号备注：`@李培尚(李培尚-东区销售部-售后客服)`。
///
/// 换成固定占位而不是映射成 `@平台A` —— `mentions[]` 只有 `easyUserId`、**没有姓名
/// 字段**，映射只能靠「第 k 个 @名字 ↔ 第 k 个 mention」的位置对应，而实测有一条是
/// 「同一个人 @ 两次」（2 个名字 1 个 mention），位置对应会给模型一个假身份。
static AT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@[^\s@(（]{1,12}(?:[(（][^)）]{0,40}[)）])?\s*").unwrap());

/// 手机号（含 `-分机` 后缀）。**两侧的非数字断言不在这个正则里** —— Rust 的 `regex`
/// 不支持后顾断言，断言由 [`phone_spans`] 手写，见那里（那两个断言是承重的）。
static PHONE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"1[3-9]\d{9}(?:-\d{4})?").unwrap());

/// [`PHONE`] 主体（不含 `-分机` 后缀）的匹配长度 —— 必须与正则 `1[3-9]\d{9}` 同步。
/// [`phone_spans`] 的回溯逻辑靠它区分「带后缀的匹配」和「裸 11 位」。
const PHONE_LEN: usize = 11;

static NEWLINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*[\r\n]+\s*").unwrap());

/// 结构化工单推送的字段名冒号是最强的锚点。
///
/// **长字段名必须排在前面**（正则交替最左优先），否则「客户姓名：贾世强」会被短的
/// 「客户」先匹配掉、只吃到「客户」两个字。
///
/// 懒惰 + 前瞻：停在分隔符 `" / "` **之前**，不把分隔前的空格一起吃掉。贪婪版会产出
/// `客户:<略>/ 手机号:` —— 少一个空格，字段名和上一个值的边界就糊了。
static FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(客户姓名|客户电话|客户旺旺|订单区域|所在地区|详细地址|收货人|收件人|联系人|客户|地址)([:：])[^/\n]*?(\s*/|$)",
    )
    .unwrap()
});

/// 三个脱敏占位符。**三个地方必须逐字一致**：[`body`] 产出它们 · [`PLACEHOLDER`]
/// 拦住 `summary` 里的它们 · `super::prompt::SYSTEM` 教模型「这不是原文、不承载关联信息」。
///
/// 不一致的代价：模型不知道那是占位符，可能拿两条都写着 `<手机号>` 的消息当同一个客户
/// （prompt 里明令禁止的事），而 validator 也拦不住它进 `summary` ——
/// **`sha256(summary)` 是 ⑤ 的缓存键，进去就焊死了**。
///
/// prompt 那一份**保持字面量**（它是逐字搬运的，改一个字所有实测结论作废），
/// 三者一致由 `the_prompt_and_the_masks_agree` 那条测试钉住。
pub(super) const MASK_PHONE: &str = "<手机号>";
pub(super) const MASK_FIELD: &str = "<略>";
pub(super) const MASK_AT: &str = "@某人";

/// `summary` 里不许出现的脱敏记号。三个占位符都不含正则元字符，直接拼。
pub(super) static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("{MASK_PHONE}|{MASK_FIELD}|{MASK_AT}")).unwrap());

/// `@名字` -> `@某人 `。**尾随空格是承重的**：`AT` 把 `@` 后的空白也吃进匹配，
/// 不补回来就会把占位符和下一个字粘住。
static AT_TO: LazyLock<String> = LazyLock::new(|| format!("{MASK_AT} "));

/// `FIELD` 的替换模板：字段名 + 冒号照抄（第 1、2 组），值换成 `<略>`，
/// 分隔符原样吐回（第 3 组，Rust 没有零宽前瞻的代偿）。
static FIELD_TO: LazyLock<String> = LazyLock::new(|| format!("${{1}}${{2}}{MASK_FIELD}${{3}}"));

/// 手机号的匹配区间 —— **两侧非数字断言是承重的**（ADR-0001:36）。
///
/// 19 位订单号 `5127366458053009229` 内部含 `1273664580` 这样的 10 位串，
/// 不加断言会把订单号掩掉半截 —— 而订单号是便签唯一可靠的关联键。
/// 1850 条实测：无断言匹配 565 处，加断言后 202 处，**差额 363 处全在订单号内部**。
///
/// Rust 的 `regex` 不支持 `(?<!\d)`，所以断言手写在 [`phone_spans`]（脱敏与 `summary`
/// 校验共用同一份实现）；连带要复刻 Python 正则引擎的两处回溯行为，各自有测试钉着：
///   * 命中被拒 → 从 `start + 1` 重新找，不是从 `end`（引擎是挪一格再试）；
///   * 带 `-1234` 后缀但其后仍是数字 → 退回不带后缀那种匹配。
///
/// 全掩不留后 4 位：模型不做按号码的跨消息关联（那是订单号的活），后 4 位不买到任何
/// 语义，只是把泄漏面从 11 位缩到 4 位 —— 代码量一样，收益不一样。
fn phone_spans(s: &str) -> Vec<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut spans = Vec::new();
    let mut pos = 0usize;
    while pos < s.len() {
        let Some(m) = PHONE.find(&s[pos..]) else {
            break;
        };
        let start = pos + m.start();
        let mut end = pos + m.end();
        // 带后缀却后接数字 -> 退回不带后缀（Python 引擎会为了满足 (?!\d) 而回溯）
        if end - start > PHONE_LEN && end < bytes.len() && bytes[end].is_ascii_digit() {
            end = start + PHONE_LEN;
        }
        let left_ok = start == 0 || !bytes[start - 1].is_ascii_digit();
        let right_ok = end >= bytes.len() || !bytes[end].is_ascii_digit();
        if left_ok && right_ok {
            spans.push((start, end));
            pos = end;
        } else {
            // 匹配必以 ASCII '1' 开头，所以 start + 1 一定是字符边界
            pos = start + 1;
        }
    }
    spans
}

fn phone_mask(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0usize;
    for (a, b) in phone_spans(s) {
        out.push_str(&s[last..a]);
        out.push_str(MASK_PHONE);
        last = b;
    }
    out.push_str(&s[last..]);
    out
}

/// 第一个手机号的原文 —— `summary` 校验用（和脱敏共用同一套边界规则）。
pub(super) fn first_phone(s: &str) -> Option<&str> {
    phone_spans(s).first().map(|&(a, b)| &s[a..b])
}

/// 模型看到的消息正文 —— **把正文交给模型的唯一出口**（承重不变量 7）。
///
/// **顺序是承重的**（ADR-0001）：引用块必须最先删（否则先掩码再整块删是白做）；
/// `@` 替换和折行在它之后（引用块尾部带 `@`，且它是多行正文的主要来源）；
/// **折行必须在 [`FIELD`] 之前**，因为 `FIELD` 的右边界就是折行产出的 `" / "`。
///
/// **只掩锚点确定的五件事** —— 判据是「这个位置的东西**必然**是什么」，不是
/// 「**看起来像**什么」。姓名和自由文本地址一概不碰：中文里动作和人名字形完全一样
/// （`改电话` / `王小宾` 都是 3 个汉字），只能靠黑名单区分，而黑名单总会漏。
/// 掩错的代价不是隐私（掩多了不泄漏），是**把一个动作变成一个人** —— 模型再也看不出
/// 商家要「改电话」，这条诉求就漏抽或抽错类型了。取舍：**宁可留 PII，不可破坏语义**。
///
/// **不做长度截断**：实测正文 P50 8 字符 / P99 213 / max 254。真出现超长正文，
/// 自适应二分会切到单条再显式失败 —— 那比静默截断好。
/// **订单号一个字符都不动**：它是业务标识不是个人信息，且便签靠它关联。
pub(super) fn body(m: &Message) -> String {
    let s = QUOTE.replace(&m.text, "");
    let s = AT.replace_all(&s, AT_TO.as_str());
    let s = phone_mask(&s);
    let s = NEWLINE.replace_all(&s, " / ");
    // ⚠️ Python 那边第三段是**零宽前瞻** `(?=\s*/|$)`；Rust 的 regex 没有前瞻，
    // 所以把分隔符吃进第 3 组再原样吐回去（`${3}`，见 [`FIELD_TO`]）。
    // 停在同一个位置，产出逐字节相同。
    let s = FIELD.replace_all(&s, FIELD_TO.as_str());
    s.trim().to_string()
}

// 错一条就是 PII 明文出境，或者订单号被掩掉半截。
// （占位符与 prompt 的一致性是跨文件测试，在 `extract/tests.rs`。）
#[cfg(test)]
mod tests {
    use super::super::tests::msgs;
    use super::*;

    /// 只为跑 `body` 的用例：一条消息，正文随便换。
    fn one(text: &str) -> Message {
        let mut m = msgs(1).remove(0);
        m.text = text.into();
        m
    }

    fn b(text: &str) -> String {
        body(&one(text))
    }

    #[test]
    fn order_numbers_are_found_mid_body_and_phones_are_not_mistaken_for_them() {
        let hit = |s: &str| ORDER_NO.find(s).map(|m| m.as_str().to_string());
        assert_eq!(
            hit("5127366458053009229  加14个筒灯").as_deref(),
            Some("5127366458053009229")
        );
        assert_eq!(
            hit("JDLY202606271814212465\n安排师傅").as_deref(),
            Some("JDLY202606271814212465")
        );
        assert_eq!(
            hit("3316977912130066680====这个餐厅反馈频闪").as_deref(),
            Some("3316977912130066680")
        );
        // 平台的工单推送把单号写在正文中间 —— 锚了行首这 36 条一条都进不了便签
        assert_eq!(
            hit("工单原因：电话核实\n订单号:JDLY202608031734008496").as_deref(),
            Some("JDLY202608031734008496")
        );
        assert_eq!(
            hit("三方：5127681781169041222").as_deref(),
            Some("5127681781169041222")
        );
        assert_eq!(
            hit("18187841287  客户三个安装单换个师傅"),
            None,
            "11 位手机号不能当订单号"
        );
        assert_eq!(
            hit("客户电话 18187841287 打不通"),
            None,
            "手机号在正文中间也不能当订单号"
        );
        assert_eq!(hit("加14个筒灯"), None);
        assert_eq!(hit("6954604 下个保护拆"), None, "6 位数字不是单号");
    }

    #[test]
    fn body_deletes_the_quote_block_with_the_name_and_phone_inside_it() {
        let q = "\"王鸿江：\nJDLY202606271814212465\n\n\n安排师傅去换配件，\
                 客户电话：17379865588，费用算我们\"\n------\n单子已经安排好了哦@王鸿江  ";
        assert_eq!(b(q), "单子已经安排好了哦@某人");
        assert!(
            !b(q).contains("王鸿江") && !b(q).contains("17379865588"),
            "引用块里的姓名/手机号漏出去了"
        );
    }

    #[test]
    fn body_replaces_at_mentions_including_the_parenthesised_note() {
        assert_eq!(b("@李晶  "), "@某人");
        assert_eq!(b("@丁家乐  @丁家乐  "), "@某人 @某人");
        assert_eq!(
            b("@李培尚(李培尚-东区销售部-售后客服)  加三四十"),
            "@某人 加三四十"
        );
    }

    #[test]
    fn body_masks_phones_including_the_extension_suffix() {
        assert_eq!(
            b("这个号码，让师傅联系处理一下18903170081"),
            "这个号码，让师傅联系处理一下<手机号>"
        );
        assert_eq!(b("手机号:18472625055-3934"), "手机号:<手机号>");
        // 师傅姓名不动 —— 只有手机号被掩（姓名规则已删，ADR-0001）
        assert_eq!(
            b("转给这个师傅：杨师傅 13289149875"),
            "转给这个师傅：杨师傅 <手机号>"
        );
    }

    /// ADR-0001:36 那 363 处差额。手写的两侧断言就是为了这一条。
    #[test]
    fn phone_masking_never_touches_a_single_character_of_an_order_number() {
        for o in [
            "5127366458053009229",
            "JDLY202608031734008496",
            "3316977912130066680",
            "3593403004800240",
            "1836102489781612345",
        ] {
            let got = b(&format!("{o} 加14个筒灯"));
            assert_eq!(got, format!("{o} 加14个筒灯"), "订单号被动了");
            assert_eq!(
                ORDER_NO.find(&got).unwrap().as_str(),
                o,
                "掩码后订单号提不出来了"
            );
        }
        assert_eq!(
            ORDER_NO
                .find(&b("5127366458053009229  淘宝 维修 / 袁柳，13581496310"))
                .unwrap()
                .as_str(),
            "5127366458053009229",
            "脱敏动了订单号"
        );
    }

    #[test]
    fn body_folds_every_newline_so_one_message_is_always_one_line() {
        // 正文冒充行框架是承重不变量 6（溯源）的绕过路径
        let addr = "3298291251974226652  淘宝 维修\n\n王小宾，15836102489-7818，河南省 新乡市";
        assert_eq!(
            b(addr),
            "3298291251974226652  淘宝 维修 / 王小宾，<手机号>，河南省 新乡市"
        );
        assert!(
            !b(addr).contains('\n') && !b(addr).contains('\r'),
            "折行没折干净"
        );

        let ms = msgs(200);
        assert!(
            ms.iter()
                .all(|m| !body(m).contains('\n') && !body(m).contains('\r')),
            "有正文仍是多行"
        );
    }

    #[test]
    fn body_does_not_truncate() {
        assert_eq!(b(&"啊".repeat(254)).chars().count(), 254, "不该截断");
    }

    #[test]
    fn body_masks_anchored_fields_only_and_keeps_the_separator_spacing() {
        assert_eq!(
            b("客户:栗子 / 手机号:13581496310 / 地址:湖南省长沙市天心区花语江南7栋1303"),
            "客户:<略> / 手机号:<手机号> / 地址:<略>",
            "「手机号」不在 _FIELD 表里 —— 它的值由 PHONE 掩成 <手机号>，不是 <略>"
        );
        // 分隔符两侧的空格必须留着 —— 值换成 <略> 之后字段名和上一个值不能糊在一起
        assert_eq!(b("客户:栗子 / 地址:河南新乡"), "客户:<略> / 地址:<略>");
        // 长字段名排在短的前面，否则「客户姓名：贾世强」只吃掉「客户」二字
        assert!(
            !b("客户姓名：贾世强").contains("贾世强"),
            "长字段名没优先匹配，姓名漏出去了"
        );
    }

    /// 删掉姓名规则的全部理由：中文里动作和人名字形完全一样，掩错就把一个动作变成一个人。
    #[test]
    fn body_rewrites_zero_business_verbs() {
        for verb in ["改电话", "指派", "转师傅", "改", "电话", "改地址"] {
            let got = b(&format!("{verb} 13581496310"));
            assert_eq!(got, format!("{verb} <手机号>"), "业务动词被改写了");
        }
        // 自由文本里的姓名/地址原样保留 —— 有意为之，不是漏了
        assert_eq!(
            b("王小宾，15836102489，河南省 新乡市 牧野区 大桥云锦府20号楼302"),
            "王小宾，<手机号>，河南省 新乡市 牧野区 大桥云锦府20号楼302"
        );
        assert_eq!(b("指派 孙师傅 13581496310"), "指派 孙师傅 <手机号>");
    }
}
