//! 解析内核第 5 层：数值与单位抽取（原值原单位 + 原文位置）。
//!
//! **这一层解决什么**：强转内核要判定「这条测值走哪条换算路径」，前提是
//! **原值与原单位原样到手** ——
//! 单位词面丢了（「3.2」是 cm 还是 mm 就无从判定，猜一个就是差 10 倍），
//! 失败也定位不到具体位置（运维只能对着报告猜是哪一行）。
//! 本层把「数值 + 单位」这一枚合并 token 拆成三样可核对的东西：
//! **原值数值**（`value_raw`）、**原单位词面**（`unit_raw`）、**原文位置**（1 基行列）。
//!
//! **四条纪律**：
//!
//! 1. **原样保留，不在这一层改写** —— 单位词面照原文保留（`CM` / `㎝` / `cm³` 都不归一；
//!    归一是单位目录那一层的职责）。强转内核按**原词面**查目录，查不到就是查不到。
//! 2. **缺单位不猜** —— 数值没有单位时产出 `UNIT_MISSING` 失败条目，**保留原值**
//!    （运维要看到「哪个数没单位」，不是看到一句「失败了」），但**不产出数值**。
//! 3. **坏值显式归类** —— 空值 / 纯符号 / 多个小数点 / 超长数字串各有**细分归类**
//!    （[`ValueDefect`]），统一落到 `VALUE_UNPARSEABLE` 错误码；
//!    猜一个「差不多」的值，会让人以为这份报告已经解析过了。
//! 4. **位置不编造** —— 位置不可确定时按 [`super::locate::locate`] 的口径返回 0；
//!    数值 token 与单位 token 各留自己的 1 基字符光标。
//!
//! **机制溯源（改写，不复制源代码）**：源产品的扫描层给每枚 token 留**字节偏移**
//! （`Token.location`），错误位置再由 `scanner_errposition` 换算成 1 基字符光标，
//! 位置未知时返回 0（不编造）。本项目把同一契约落到中文报告上：
//! 数值 token 与单位 token 各留 1 基字符光标，失败条目直接指回原文第几行第几列。
//!
//! **本层是数值可解析性的唯一真源**：合并层判定「这个数字能不能用」时也走
//! [`parse_measurement_value`]，不在两处各写一套规则（两处真源必然漂移，
//! 漂移表现为「同一份报告在不同机器上得到不同数值」）。

use super::errors::{DetailCode, ErrorCode, Failure};
use super::lexer::{is_fullwidth_digit, to_halfwidth_digit, TokenKind};
use super::locate::locate;
use super::lookahead::Merged;

/// 数值词面最大长度（字符）。
///
/// 15 位是**院内口径的常识上限**：长度类测量写「85」「9.5」「0.123456」都远小于它。
/// 超过它多半是扫描件串行 / 复制粘贴粘连出来的数字串 —— 把它当数值收下，
/// 台账里会出现一个**看着正常、实际是乱码**的数字，比缺这一行更难发现。
pub const MAX_VALUE_LEN: usize = 15;

/// 纯符号词面里的符号字符（正负号 / 小数点 / 乘号）。
const SIGN_CHARS: [char; 6] = ['+', '-', '\u{2212}', '.', '×', '＊'];

/// 小数点（半角与全角）。
const DOT_CHARS: [char; 2] = ['.', '\u{ff0e}'];

/// 负号（半角减号 / Unicode 减号 / 全角连字符 / 小型连字符）。
///
/// **破折号与连接号不算负号**：`3.2-4.1cm` 里的连字符是区间分隔符，
/// 写成 `3.2—4.1cm` 也是区间 —— 只有真正带符号位的减号才进这一集。
const MINUS_CHARS: [char; 4] = ['-', '\u{2212}', '\u{ff0d}', '\u{fe63}'];

/// 倍率写法里的乘号（`3.2×2` / `3.2x2`）。
const SCALE_MARKS: [char; 2] = ['×', 'x'];

/// 数值书写缺陷（`VALUE_UNPARSEABLE` 错误码下的**细分归类**）。
///
/// 错误码要稳定（运维按码聚合），细分归类要具体（运维要知道**哪一种**坏）——
/// 两者不冲突：码进台账与失败清单，归类进明细与摘要。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueDefect {
    /// 空值：数值词面为空。
    Empty,
    /// 纯符号：词面只有符号（`+` / `-` / `.` / `×`），一个数字都没有。
    PureSign,
    /// 多个小数点：`3.2.5`。
    MultipleDots,
    /// 超长数字串：长度超过 [`MAX_VALUE_LEN`]。
    TooLong,
    /// 含非数值字符。
    NotNumeric,
}

impl ValueDefect {
    /// 细分归类（登记在 [`DetailCode`] 表里 —— **唯一真源**）。
    ///
    /// 本层只判「是哪种坏」，**不自带一套码与文案** —— 码表只有一处，
    /// 两处各写一份必然漂移（漂移表现为：失败清单按码聚合时同一类坏分成两堆）。
    pub fn detail_code(self) -> DetailCode {
        match self {
            ValueDefect::Empty => DetailCode::ValueEmpty,
            ValueDefect::PureSign => DetailCode::ValuePureSign,
            ValueDefect::MultipleDots => DetailCode::ValueMultipleDots,
            ValueDefect::TooLong => DetailCode::ValueTooLong,
            ValueDefect::NotNumeric => DetailCode::ValueNotNumeric,
        }
    }

    /// 细分归类字符串（进失败明细与运行摘要）。
    pub fn as_str(self) -> &'static str {
        self.detail_code().as_str()
    }

    /// 中文文案（失败清单与运行摘要直接展示）。
    pub fn message(self) -> &'static str {
        self.detail_code().message()
    }
}

/// 数值解析结果。
#[derive(Clone, Debug, PartialEq)]
pub enum ValueParse {
    /// 解析成功（数值）。
    Ok(f64),
    /// 不可解析 —— 带细分缺陷。
    Bad(ValueDefect),
}

impl ValueParse {
    /// 解析出的数值（不可解析为 `None`）。
    pub fn value(&self) -> Option<f64> {
        match self {
            ValueParse::Ok(v) => Some(*v),
            ValueParse::Bad(_) => None,
        }
    }

    /// 细分缺陷（解析成功为 `None`）。
    pub fn defect(&self) -> Option<ValueDefect> {
        match self {
            ValueParse::Ok(_) => None,
            ValueParse::Bad(d) => Some(*d),
        }
    }

    /// 是否解析成功。
    pub fn is_ok(&self) -> bool {
        matches!(self, ValueParse::Ok(_))
    }
}

/// 解析数值词面（`3.2` / `32` / `120` / `.5`）。
///
/// **四类书写缺陷各有明确的归类**（统一落 `VALUE_UNPARSEABLE`，但归类不同）：
///
/// | 缺陷 | 词面 | 归类 |
/// |---|---|---|
/// | 空值 | `""` | [`ValueDefect::Empty`] |
/// | 纯符号 | `"-"` / `"."` | [`ValueDefect::PureSign`] |
/// | 多个小数点 | `"3.2.5"` | [`ValueDefect::MultipleDots`] |
/// | 超长数字串 | 长度 > [`MAX_VALUE_LEN`] | [`ValueDefect::TooLong`] |
///
/// 全角数字先归一为半角再判定 —— 全角写法与半角写法说的是同一件事，
/// 不能因为书写形式不同就判成两种结果。
pub fn parse_measurement_value(raw: &str) -> ValueParse {
    let normalized: String = raw
        .trim()
        .chars()
        .map(|c| {
            if is_fullwidth_digit(c) {
                to_halfwidth_digit(c)
            } else {
                c
            }
        })
        .collect();

    if normalized.is_empty() {
        return ValueParse::Bad(ValueDefect::Empty);
    }
    // 纯符号：一个数字都没有，只有符号 —— 不猜它「想写」什么。
    if normalized.chars().all(|c| SIGN_CHARS.contains(&c)) {
        return ValueParse::Bad(ValueDefect::PureSign);
    }
    if normalized.chars().filter(|c| *c == '.').count() > 1 {
        return ValueParse::Bad(ValueDefect::MultipleDots);
    }
    if normalized.chars().count() > MAX_VALUE_LEN {
        return ValueParse::Bad(ValueDefect::TooLong);
    }
    if !normalized.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return ValueParse::Bad(ValueDefect::NotNumeric);
    }
    match normalized.parse::<f64>() {
        Ok(v) => ValueParse::Ok(v),
        Err(_) => ValueParse::Bad(ValueDefect::NotNumeric),
    }
}

/// 一条数值候选（合并 token 里与「数值 / 单位」有关的一枚）。
#[derive(Clone, Debug)]
pub struct ValueHit {
    /// 原值数值（**未含倍率**；倍率另见 [`ValueHit::scale`]）。
    ///
    /// 不可解析时为 `None` —— 不产出「差不多」的值。
    pub value: Option<f64>,
    /// 原值词面（**原文写法**，如 `3.2` / `3.2×2` / `.5`）—— 失败条目回显它。
    pub value_text: String,
    /// 原单位词面（**原文写法**，未知单位也保留）；缺单位为 `None`。
    pub unit: Option<String>,
    /// 倍率（原值词面自带 `×2` 时为 `Some`）。
    pub scale: Option<f64>,
    /// 数值 token 的 1 基字符光标。
    pub start: usize,
    /// 单位 token 的 1 基字符光标（缺单位为 `None`）。
    pub unit_start: Option<usize>,
    /// 1 基行号（未知 0）。
    pub line: usize,
    /// 1 基列号（未知 0）。
    pub column: usize,
}

impl ValueHit {
    /// 有效原值（倍率折算后的数值）。
    ///
    /// 台账里的 `value_raw` 是**有效原值**：原文写 `3.2×2cm` 时，原值就是 `6.4cm`
    /// —— 倍率是原文的一部分，不是单位的换算，强转路径另记 `DirectScale`。
    pub fn effective_value(&self) -> Option<f64> {
        self.value.map(|v| v * self.scale.unwrap_or(1.0))
    }

    /// 单位词面（缺单位为 `""`）。
    pub fn unit_text(&self) -> &str {
        self.unit.as_deref().unwrap_or("")
    }
}

/// 数值层对一枚合并 token 的判定。
#[derive(Clone, Debug)]
pub enum ValueVerdict {
    /// 不是数值 token（部位 / 测项 / 叙述 / 标点）—— 不参与数值抽取。
    NotAValue,
    /// 数值 + 单位齐备：可以交给强转内核。
    Measured(ValueHit),
    /// 数值有、单位缺：**保留原值**，不产出数值。
    MissingUnit(ValueHit),
    /// 数值不可解析：不产出数值。
    Unparseable(ValueHit),
}

/// 判定合并 token 序列里的每一枚与「数值 / 单位」有关的 token。
///
/// 与 [`super::lookahead::Merged`] **逐位对应**（同长度、同次序），
/// 调用方按位置取用判定结果即可，不必自己在两处各认一遍 token。
///
/// **只有本层认得的写法才判数值**：单位词面照原文保留（不归一），
/// 缺单位与坏值各有各的失败码，不合并成一句「解析失败」。
pub fn classify_values(merged: &[Merged], text: &str) -> Vec<ValueVerdict> {
    let mut out: Vec<ValueVerdict> = Vec::with_capacity(merged.len());
    for (i, m) in merged.iter().enumerate() {
        // 前缀（**本枚之前**的合并 token）：判定「前置小数点」「前导负号」
        // 都要回看上一枚 —— 词法层把符号与小数点切成独立标点 token，
        // 数值解释的责任在本层。
        out.push(verdict_of(m, &merged[..i], text));
    }
    out
}

/// 单枚合并 token 的判定。
fn verdict_of(m: &Merged, prefix: &[Merged], text: &str) -> ValueVerdict {
    let prev = prefix.last();
    match m {
        Merged::Measurement {
            value,
            scale,
            unit,
            raw_value,
            start,
            unit_start,
        } => {
            // 前置小数点（`.5cm`）：还原成 ".5" 再解释，否则差 10 倍。
            let dotted = leading_dot_literal(prev, *start, raw_value);
            let (value, value_text) = match dotted {
                Some(literal) => match parse_measurement_value(&literal) {
                    ValueParse::Ok(v) => (v, literal),
                    // 还原不出来就沿用合并层给的值（词法层已保证是数字，
                    // 走不到这里；真走进来也不能凭此制造一条假失败）。
                    ValueParse::Bad(_) => (*value, raw_value.clone()),
                },
                None => (*value, raw_value.clone()),
            };
            // 前导负号（`-3.2cm`）：符号保留进数值，不在这里静默当成正数。
            let (value, value_text, hit_start) = apply_sign(prefix, value, value_text, *start, text);
            ValueVerdict::Measured(build_hit(
                Some(value),
                value_text,
                Some(unit.clone()),
                *scale,
                hit_start,
                Some(*unit_start),
                text,
            ))
        }
        Merged::BadNumber {
            raw_value,
            unit,
            start,
        } => ValueVerdict::Unparseable(build_hit(
            None,
            raw_value.clone(),
            if unit.is_empty() {
                None
            } else {
                Some(unit.clone())
            },
            None,
            *start,
            None,
            text,
        )),
        Merged::Scale {
            value,
            factor,
            raw_value,
            start,
        } => {
            // 倍率链没走完（`3.2×` / `3.2×2` 后面没有单位）：原值保留，不产出数值。
            let (value, value_text, hit_start) =
                apply_sign(prefix, *value, raw_value.clone(), *start, text);
            ValueVerdict::MissingUnit(build_hit(
                Some(value),
                value_text,
                None,
                *factor,
                hit_start,
                None,
                text,
            ))
        }
        Merged::Raw(tok) if tok.kind == TokenKind::Number => {
            let dotted = leading_dot_literal(prev, tok.start, &tok.raw);
            let (value, value_text) = match dotted {
                Some(literal) => match parse_measurement_value(&literal) {
                    ValueParse::Ok(v) => (v, literal),
                    ValueParse::Bad(_) => (0.0, tok.raw.clone()),
                },
                None => match parse_measurement_value(&tok.text) {
                    ValueParse::Ok(v) => (v, tok.raw.clone()),
                    ValueParse::Bad(_) => {
                        return ValueVerdict::Unparseable(build_hit(
                            None,
                            tok.raw.clone(),
                            None,
                            None,
                            tok.start,
                            None,
                            text,
                        ))
                    }
                },
            };
            // 前导负号（`-3.2`）：符号保留进数值，不静默当成正数。
            let (value, value_text, hit_start) = apply_sign(prefix, value, value_text, tok.start, text);
            // 数值后面没有单位：不猜单位，不产出数值 —— 但**保留原值**，
            // 运维要看到「哪个数没单位」。
            ValueVerdict::MissingUnit(build_hit(
                Some(value),
                value_text,
                None,
                None,
                hit_start,
                None,
                text,
            ))
        }
        _ => ValueVerdict::NotAValue,
    }
}

/// 构造数值候选（行号 / 列号由字符光标换算，位置未知为 0）。
fn build_hit(
    value: Option<f64>,
    value_text: String,
    unit: Option<String>,
    scale: Option<f64>,
    start: usize,
    unit_start: Option<usize>,
    text: &str,
) -> ValueHit {
    let (line, column) = locate(text, start);
    ValueHit {
        value,
        value_text,
        unit,
        scale,
        start,
        unit_start,
        line,
        column,
    }
}

/// 前一枚 token 是**紧邻的小数点**时，把词面还原成 `.5` 这种写法。
///
/// 「.5cm」被读成 5cm 是**差 10 倍的静默错误** —— 台账里的数字看着完全正常，
/// 对拍也照常进行，只是比错了对象。词法层把小数点切成独立标点 token，
/// 数值解释的责任在本层。
///
/// 只在**紧邻**时还原（中间有空格就不是小数点了）；倍率写法只还原数值部分
/// （`.5×2` → 数值部分 `.5`、倍率 `×2`）。
fn leading_dot_literal(prev: Option<&Merged>, start: usize, raw: &str) -> Option<String> {
    let Merged::Raw(tok) = prev? else {
        return None;
    };
    if tok.kind != TokenKind::Punct {
        return None;
    }
    let first = tok.text.chars().next()?;
    if !DOT_CHARS.contains(&first) {
        return None;
    }
    // 必须紧邻：标点 token 的 end 与数值 token 的 start 相邻才算一个小数点。
    if tok.end + 1 != start {
        return None;
    }
    let (head, _tail) = split_scale_literal(raw);
    Some(format!(".{}", head))
}

/// 原值词面拆成「数值部分 / 倍率部分」（`3.2×2` → `("3.2", "×2")`）。
fn split_scale_literal(raw: &str) -> (&str, Option<&str>) {
    match raw.char_indices().find(|(_, c)| SCALE_MARKS.contains(c)) {
        Some((i, _)) => (&raw[..i], Some(&raw[i..])),
        None => (raw, None),
    }
}

/// 前导负号：词法层把 `-` 切成独立标点 token，**数值解释的责任在本层**。
///
/// 「-3.2cm」被读成 32mm 是**静默改数** —— 台账里的数字看着完全正常，
/// 只是符号没了；对拍也照常进行，只是比错了对象。
/// 负值不是测量值（强转内核的 `is_measurement_value` 拒绝负数），
/// 所以这里把符号**保留进数值**：交给强转内核判失败 ——
/// 条目仍在（原值 `-3.2` 留痕），但不产出归一值、不静默当成正数。
///
/// **只在紧邻时生效**（中间有空格就不是符号了），且**负号前面不是数字** ——
/// `3.2-4.1cm` 里的连字符是区间分隔符，4.1 不是 -4.1。
///
/// 返回负号所在的 1 基字符光标（没有负号返回 `None`）。
fn leading_sign(prefix: &[Merged], start: usize, text: &str) -> Option<usize> {
    let mut cursor = start;
    let mut idx = prefix.len();
    while idx > 0 {
        let tok = match &prefix[idx - 1] {
            Merged::Raw(tok) if tok.kind == TokenKind::Punct => tok,
            _ => return None,
        };
        // 必须紧邻：标点 token 的 end 与数值 token 的 start 相邻才算前置符号。
        if tok.end + 1 != cursor {
            return None;
        }
        let first = tok.text.chars().next()?;
        if MINUS_CHARS.contains(&first) {
            // 负号前面是数字 → 这是区间分隔符（「3.2-4.1cm」），不是符号。
            return if prev_char_is_digit(tok.start, text) {
                None
            } else {
                Some(tok.start)
            };
        }
        // 其他紧邻标点（如小数点）→ 继续往前回看（「-.5cm」）。
        cursor = tok.start;
        idx -= 1;
    }
    None
}

/// 负号前一个字符是不是数字（区分「符号」与「区间分隔符」）。
fn prev_char_is_digit(start: usize, text: &str) -> bool {
    if start < 2 {
        return false;
    }
    text.chars()
        .nth(start - 2)
        .map(|c| is_fullwidth_digit(c) || c.is_ascii_digit())
        .unwrap_or(false)
}

/// 前导负号落到数值上（没有负号时原样返回）。
///
/// 返回 `(数值, 原值词面, 起始字符光标)`：
/// 有负号时数值取反、词面补上负号、光标移到负号处
/// —— 失败片段从光标处起可读（「-3.2」正好对上负号那一列）。
fn apply_sign(
    prefix: &[Merged],
    value: f64,
    value_text: String,
    start: usize,
    text: &str,
) -> (f64, String, usize) {
    match leading_sign(prefix, start, text) {
        Some(minus_pos) => (-value, format!("-{}", value_text), minus_pos),
        None => (value, value_text, start),
    }
}

/// 缺单位 → 失败条目（**保留原值**：运维要看到是哪个数没单位）。
///
/// 位置指回**数值 token 自身**（1 基行列由字符光标换算）。
pub fn unit_missing_failure(hit: &ValueHit, text: &str) -> Failure {
    let (line, column) = locate(text, hit.start);
    Failure::new(
        ErrorCode::UnitMissing,
        hit.start,
        line,
        column,
        &hit.value_text,
    )
}

/// 数值不可解析 → 失败条目（带**细分归类**的说明）。
///
/// 片段是「原值词面 + 原单位词面」（`3.2.5cm`）—— 运维照着这一段就能在原报告里
/// 找到出问题的写法；只有值没有单位时片段就是值本身。
///
/// **片段只放原文**：细分归类（`VALUE_MULTIPLE_DOTS` 这类）走
/// [`Failure::detail`]，不掺进片段 —— 片段是照着原文找的锚点，掺别的东西就锚不准。
pub fn value_unparseable_failure(hit: &ValueHit, text: &str) -> Failure {
    let (line, column) = locate(text, hit.start);
    let snippet = format!("{}{}", hit.value_text, hit.unit_text());
    let defect = parse_measurement_value(&hit.value_text)
        .defect()
        .unwrap_or(ValueDefect::NotNumeric);
    Failure::new(
        ErrorCode::ValueUnparseable,
        hit.start,
        line,
        column,
        &snippet,
    )
    .with_detail_code(defect.detail_code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::lexer::tokenize;
    use crate::parse::lookahead::merge;

    /// 合并后的 token 序列（用例入口）。
    fn merged(text: &str) -> Vec<Merged> {
        merge(&tokenize(text))
    }

    /// 数值层判定（用例入口）。
    fn verdicts(text: &str) -> Vec<ValueVerdict> {
        classify_values(&merged(text), text)
    }

    /// 第一条测量候选（用例入口）。
    fn measured(text: &str) -> ValueHit {
        verdicts(text)
            .into_iter()
            .find_map(|v| match v {
                ValueVerdict::Measured(h) => Some(h),
                _ => None,
            })
            .expect("应产出测量候选")
    }

    /// 第一条缺单位候选（用例入口）。
    fn missing(text: &str) -> ValueHit {
        verdicts(text)
            .into_iter()
            .find_map(|v| match v {
                ValueVerdict::MissingUnit(h) => Some(h),
                _ => None,
            })
            .expect("应产出缺单位候选")
    }

    /// 第一条不可解析候选（用例入口）。
    fn unparseable(text: &str) -> ValueHit {
        verdicts(text)
            .into_iter()
            .find_map(|v| match v {
                ValueVerdict::Unparseable(h) => Some(h),
                _ => None,
            })
            .expect("应产出不可解析候选")
    }

    #[test]
    fn 三点二厘米抽出原值原单位与原文位置() {
        let hit = measured("左肾 3.2cm");
        assert_eq!(hit.value, Some(3.2));
        assert_eq!(hit.value_text, "3.2");
        assert_eq!(hit.unit.as_deref(), Some("cm"));
        assert_eq!(hit.effective_value(), Some(3.2));
        // 「左肾 3.2cm」：数值从第 4 个字符起，单位从第 7 个字符起。
        assert_eq!(hit.start, 4);
        assert_eq!(hit.unit_start, Some(7));
        assert_eq!(hit.line, 1);
        assert_eq!(hit.column, 4);
    }

    #[test]
    fn 数值与单位之间留空格照样抽得出() {
        let hit = measured("左肾积水 3.2 cm");
        assert_eq!(hit.value, Some(3.2));
        assert_eq!(hit.unit.as_deref(), Some("cm"));
        assert_eq!(hit.value_text, "3.2");
    }

    #[test]
    fn 三十二毫米抽得出且单位原样保留() {
        let hit = measured("左肾长 32mm");
        assert_eq!(hit.value, Some(32.0));
        assert_eq!(hit.unit.as_deref(), Some("mm"));
    }

    #[test]
    fn 残余尿量约一百二十毫升抽得出() {
        // 「约」是修饰词，不影响数值与单位。
        let hit = measured("膀胱残余尿量 约 120ml");
        assert_eq!(hit.value, Some(120.0));
        assert_eq!(hit.unit.as_deref(), Some("ml"));
        assert_eq!(hit.value_text, "120");
    }

    #[test]
    fn 未知单位照样保留原词面不猜不换() {
        // 「cc」不在单位目录里：原单位照样交出去，换算路径由强转内核判无路径。
        let hit = measured("左肾积水 3.2cc");
        assert_eq!(hit.unit.as_deref(), Some("cc"));
        assert_eq!(hit.value, Some(3.2));
    }

    #[test]
    fn 缺单位保留原值且不产出数值() {
        let hit = missing("左肾积水 3.2");
        assert_eq!(hit.value, Some(3.2), "原值要保留");
        assert_eq!(hit.value_text, "3.2");
        assert_eq!(hit.unit, None);
        assert_eq!(hit.effective_value(), Some(3.2));
        let failure = unit_missing_failure(&hit, "左肾积水 3.2");
        assert_eq!(failure.code, ErrorCode::UnitMissing);
        assert_eq!(failure.raw_snippet, "3.2");
        assert_eq!(failure.line, 1);
        assert_eq!(failure.column, 6);
    }

    #[test]
    fn 倍率链没走完按缺单位处理() {
        // 「3.2×」后面没有单位：不猜单位，原值保留。
        let hit = missing("左肾 3.2×");
        assert_eq!(hit.unit, None);
        assert_eq!(hit.value_text, "3.2×");
    }

    #[test]
    fn 多个小数点判为不可解析并带细分归类() {
        let hit = unparseable("左肾积水 3.2.5cm");
        assert_eq!(hit.value, None);
        assert_eq!(hit.value_text, "3.2.5");
        assert_eq!(hit.unit.as_deref(), Some("cm"));
        let failure = value_unparseable_failure(&hit, "左肾积水 3.2.5cm");
        assert_eq!(failure.code, ErrorCode::ValueUnparseable);
        // 片段**只放原文**（照着这一段能在原报告里找到它）；
        // 细分归类另走 detail，错误码保持稳定不变。
        assert_eq!(failure.raw_snippet, "3.2.5cm");
        assert_eq!(failure.detail, ValueDefect::MultipleDots.as_str());
        assert!(failure.message().contains(ValueDefect::MultipleDots.as_str()));
    }

    #[test]
    fn 超长数字串判为不可解析() {
        // 15 位以内是院内常识上限，再长多半是串行粘连。
        let long = "1".repeat(MAX_VALUE_LEN + 1);
        let text = format!("左肾积水 {}cm", long);
        let hit = unparseable(&text);
        assert_eq!(hit.value, None);
        let failure = value_unparseable_failure(&hit, &text);
        assert_eq!(failure.detail, ValueDefect::TooLong.as_str());
    }

    #[test]
    fn 空值与纯符号各有细分归类() {
        assert_eq!(
            parse_measurement_value("").defect(),
            Some(ValueDefect::Empty)
        );
        assert_eq!(
            parse_measurement_value("   ").defect(),
            Some(ValueDefect::Empty)
        );
        assert_eq!(
            parse_measurement_value("-").defect(),
            Some(ValueDefect::PureSign)
        );
        assert_eq!(
            parse_measurement_value(".").defect(),
            Some(ValueDefect::PureSign)
        );
    }

    #[test]
    fn 数值词面分类逐条断言() {
        assert_eq!(parse_measurement_value("3.2").value(), Some(3.2));
        assert_eq!(parse_measurement_value("32").value(), Some(32.0));
        assert_eq!(parse_measurement_value(".5").value(), Some(0.5));
        assert_eq!(parse_measurement_value("3.").value(), Some(3.0));
        // 全角数字归一后与半角同义。
        assert_eq!(parse_measurement_value("３.２").value(), Some(3.2));
        assert_eq!(
            parse_measurement_value("3.2.5").defect(),
            Some(ValueDefect::MultipleDots)
        );
        assert_eq!(
            parse_measurement_value("3.2cm").defect(),
            Some(ValueDefect::NotNumeric)
        );
    }

    #[test]
    fn 前置小数点还原成零点几而不是差十倍() {
        // 「.5cm」若按 5cm 收下，台账里的数字看着完全正常，只是比错了对象。
        let hit = measured("左肾积水 .5cm");
        assert_eq!(hit.value, Some(0.5));
        assert_eq!(hit.value_text, ".5");
    }

    #[test]
    fn 前置小数点与倍率同时出现也还原() {
        let hit = measured("左肾积水 .5×2cm");
        assert_eq!(hit.value, Some(0.5));
        assert_eq!(hit.scale, Some(2.0));
        assert_eq!(hit.effective_value(), Some(1.0));
    }

    #[test]
    fn 数值与单位各留自己的字符光标() {
        let hit = measured("超声所见\n左肾 3.2cm");
        assert_eq!(hit.line, 2);
        assert_eq!(hit.column, 4, "数值在第 2 行第 4 列");
        // 「超声所见」4 字 + 换行 1 字 + 「左肾 」3 字 = 数值从第 9 个字符起，
        // 单位从第 12 个字符起。
        assert_eq!(hit.start, 9);
        assert_eq!(hit.unit_start, Some(12));
    }

    #[test]
    fn 非数值token不参与数值抽取() {
        assert!(verdicts("左肾积水")
            .iter()
            .all(|v| matches!(v, ValueVerdict::NotAValue)));
    }

    #[test]
    fn 判定结果与合并序列逐位对应() {
        let text = "左肾 3.2cm 右肾积水";
        let m = merged(text);
        let v = verdicts(text);
        assert_eq!(m.len(), v.len());
    }

    #[test]
    fn 前导负号保留进数值不被丢掉() {
        // 「-3.2cm」丢掉符号就是 32mm —— 台账里是个正常得不能再正常的数字。
        let hit = measured("左肾 -3.2cm");
        assert_eq!(hit.value, Some(-3.2), "负号不许丢");
        assert_eq!(hit.value_text, "-3.2", "原值词面带号");
        assert_eq!(hit.effective_value(), Some(-3.2));
        // 光标移到负号那一列 —— 失败片段从光标处起可读。
        assert_eq!(hit.start, 4);
        assert_eq!(hit.column, 4);
    }

    #[test]
    fn 前导负号缺单位时也保留符号() {
        let hit = missing("左肾 -3.2");
        assert_eq!(hit.value, Some(-3.2));
        assert_eq!(hit.value_text, "-3.2");
    }

    #[test]
    fn 区间连字符不是负号() {
        // 「3.2-4.1cm」里的连字符是区间分隔符：4.1 不是 -4.1。
        let hit = measured("左肾 3.2-4.1cm");
        assert_eq!(hit.value, Some(4.1));
        assert_eq!(hit.value_text, "4.1");
    }

    #[test]
    fn 负号与数值不紧邻时不生效() {
        // 中间有空格就不是符号了。
        let hit = measured("左肾 - 3.2cm");
        assert_eq!(hit.value, Some(3.2));
        assert_eq!(hit.value_text, "3.2");
    }

    #[test]
    fn 全角负号与数学减号同样认得出() {
        for text in ["左肾 −3.2cm", "左肾 －3.2cm"] {
            let hit = measured(text);
            assert_eq!(hit.value, Some(-3.2), "[{text}] 负号应被认出来");
            assert_eq!(hit.value_text, "-3.2");
        }
    }

    #[test]
    fn 前置小数点带负号也还原() {
        // 「-.5cm」：小数点还原 + 负号保留，两条都不许丢。
        let hit = measured("左肾 -.5cm");
        assert_eq!(hit.value, Some(-0.5));
        assert_eq!(hit.value_text, "-.5");
    }
}
