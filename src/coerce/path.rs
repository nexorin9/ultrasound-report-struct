//! 强转内核：**量纲强转路径判定**（CoercionPath 判定内核本体）。
//!
//! 机制溯源（**新建实现**，不复制源代码）：源产品以「**合法路径枚举**」表达源值到
//! 目标口径的转换 —— 路径本身是一组枚举值，**查不到就是查不到**，不静默兜底。
//! 源侧摘录只到 seam 接口声明（`CoercionPathType` + `find_coercion_pathway_implicit`），
//! 判定算法本体未实现；本项目自行实现这套判定，语义由强转路径对照表逐条守住。
//!
//! # 四类路径
//!
//! | 路径 | 含义 | 步骤链 | 例 |
//! |---|---|---|---|
//! | `UnitConvert` | 单位族内直接换算到本族目标口径 | `UnitConvert` | `3.2cm → 32mm`；`1L → 1000mL` |
//! | `DirectScale` | 原值词面自带倍率（原文写明的 `×2`） | `Scale → UnitConvert` | `3.2×2cm → 64mm` |
//! | `Composite` | 复合量纲，需先还原量纲再换算 | `DimensionalReduce → UnitConvert` | `3.2cm³ → 3.2mL` |
//! | `None` | 查不到合法路径 | 空 | `3.2xx`（未知单位） |
//!
//! **步骤链是判定的一部分**：路径标签说「走哪一类」，步骤链说「实际走了哪几步」。
//! 两者不一致就是判定出了问题 —— [`CoercionDecision::is_consistent`] 把这条
//! 不变量写成可断言的代码，而不是靠人记。
//!
//! # 判定顺序（fail-closed，逐条可断言）
//!
//! 1. **数值必须有限** —— `NaN` / `inf` 不产出数值（`ValueInvalid`）。
//! 2. **倍率必须合法** —— 倍率非有限或 `≤ 0` 即**查不到合法路径**（`ScaleInvalid`）。
//!    倍率是原文写明的，**不猜倍率**：读不出来就没有路径。
//! 3. **单位必须查得到目录** —— 查不到就是查不到（`UnitUnknown`），
//!    **不按任何单位兜底**（`cc` 不按 `cm` 处理、`mm2` 不按 `mm` 处理）。
//! 4. **值域与换算** —— 归一值的算术交给单位目录的换算层
//!    （[`super::units::convert_to_target`]）：**负值 / 超量级 / 跨族**在那里判，
//!    **判定与换算同源** —— 不会出现「判了一条路、换算走另一条」。
//! 5. 以上都过 → 组装步骤链，按「**复合量纲优先于倍率**」定路径标签，产出归一值。
//!
//! # 复合量纲优先于倍率
//!
//! `3.2×2cm³` 同时命中复合量纲与倍率：路径标签取 `Composite`（量纲还原是**先决条件**，
//! 必须在倍率之前做），倍率以步骤链体现 —— 步骤链是
//! `DimensionalReduce → Scale → UnitConvert`（三步）。
//!
//! # 谁调用它
//!
//! - 抽取层（[`crate::parse::extract`]）拿 [`decide`] 的**一次判定结果**同时得到
//!   路径标签与归一值 —— 路径判定与实际换算**同源**，不存在「判了一条路、换算走另一条」。
//! - 强转路径对照表（Task 23）拿 [`coerce_raw`] 从**原值词面**驱动
//!   （`3.2×2` + `cm` → `DirectScale` / `64` / `mm`）。
//!
//! 本模块**不认识原文位置**：位置是解析层的事。判定失败由调用方落成失败条目
//! （错误码 `COERCION_NO_PATH`，见 [`crate::parse::errors`]）。

use super::units::{canonical_unit, convert_to_target, Family};

/// 量纲强转路径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoercionPath {
    /// 单位族内直接换算到本族目标口径。
    UnitConvert,
    /// 原值词面自带倍率，倍率折算后再单位换算。
    DirectScale,
    /// 复合量纲，需先还原量纲再单位换算。
    Composite,
    /// 查不到合法路径 —— **显式失败，不静默兜底**。
    None,
}

impl CoercionPath {
    /// **全部路径**（穷举清单，供码表自检与覆盖率断言用）。
    pub const ALL: [CoercionPath; 4] = [
        CoercionPath::UnitConvert,
        CoercionPath::DirectScale,
        CoercionPath::Composite,
        CoercionPath::None,
    ];

    /// 路径字符串（进台账行与对照表）。
    pub fn as_str(self) -> &'static str {
        match self {
            CoercionPath::UnitConvert => "UnitConvert",
            CoercionPath::DirectScale => "DirectScale",
            CoercionPath::Composite => "Composite",
            CoercionPath::None => "None",
        }
    }

    /// 路径 ↔ 字符串往返（**未知字符串返回 `None`**）。
    ///
    /// 从 CSV / 库里读回路径时用它 —— 读到表里没有的值就是**数据不对**，
    /// 宁可返回 `None` 让调用方显式处理，也不硬套一个默认路径。
    pub fn from_code(raw: &str) -> Option<CoercionPath> {
        match raw {
            "UnitConvert" => Some(CoercionPath::UnitConvert),
            "DirectScale" => Some(CoercionPath::DirectScale),
            "Composite" => Some(CoercionPath::Composite),
            "None" => Some(CoercionPath::None),
            _ => None,
        }
    }

    /// 中文标签（失败清单与运行摘要里的一行说明）。
    pub fn label_zh(self) -> &'static str {
        match self {
            CoercionPath::UnitConvert => "单位族内换算",
            CoercionPath::DirectScale => "倍率折算 + 单位族内换算",
            CoercionPath::Composite => "量纲还原 + 单位族内换算",
            CoercionPath::None => "查不到合法路径",
        }
    }

    /// 是否是失败路径（`None` 是唯一的失败路径）。
    pub fn is_failure(self) -> bool {
        matches!(self, CoercionPath::None)
    }
}

impl std::fmt::Display for CoercionPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 判定里**实际走过**的一步。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoercionStep {
    /// 量纲还原（`cm³` → 体积量纲；还原后才能谈单位换算）。
    DimensionalReduce,
    /// 倍率折算（原文写明的 `×2`，不猜倍率）。
    Scale,
    /// 单位族内换算到本族目标口径（长度 → `mm`，容量 → `mL`）。
    UnitConvert,
}

impl CoercionStep {
    /// 步骤字符串（进步骤链的对照输出）。
    pub fn as_str(self) -> &'static str {
        match self {
            CoercionStep::DimensionalReduce => "DimensionalReduce",
            CoercionStep::Scale => "Scale",
            CoercionStep::UnitConvert => "UnitConvert",
        }
    }
}

impl std::fmt::Display for CoercionStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 查不到合法路径的**原因**（同一失败码下的具体坏法）。
///
/// 运维处置动作不同：**单位未知**要补口径目录，**倍率/数值坏法**要报告侧改写法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoercionFailure {
    /// 单位不在本轮口径目录内（不按任何单位兜底）。
    UnitUnknown,
    /// 倍率非法（非有限或 `≤ 0`）—— 不猜倍率。
    ScaleInvalid,
    /// 原值词面里带倍率但倍率读不出来 —— 不猜倍率。
    ScaleUnreadable,
    /// 数值不可用 —— 非有限（`NaN` / `inf`）、**负值**、**超量级**，
    /// 或原值词面不是数值 —— 一律不产出数值。
    ValueInvalid,
}

impl CoercionFailure {
    /// 原因字符串（进判定自检输出）。
    pub fn as_str(self) -> &'static str {
        match self {
            CoercionFailure::UnitUnknown => "COERCION_UNIT_UNKNOWN",
            CoercionFailure::ScaleInvalid => "COERCION_SCALE_INVALID",
            CoercionFailure::ScaleUnreadable => "COERCION_SCALE_UNREADABLE",
            CoercionFailure::ValueInvalid => "COERCION_VALUE_INVALID",
        }
    }

    /// 中文文案（面向运维）。
    pub fn message(self) -> &'static str {
        match self {
            CoercionFailure::UnitUnknown => "单位不在本轮口径目录内，未按任何单位兜底",
            CoercionFailure::ScaleInvalid => "倍率非法（须为正数），未猜倍率",
            CoercionFailure::ScaleUnreadable => "原值词面里的倍率读不出来，未猜倍率",
            CoercionFailure::ValueInvalid => "数值不可用（须为有限、非负、不超量级的测量值），未产出数值",
        }
    }
}

impl std::fmt::Display for CoercionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次强转判定的**完整产物**。
///
/// 路径标签、归一值、单位码、步骤链、失败原因**一次判定同时给出** ———
/// 判定与换算同源，不存在「判了一条路、换算走另一条」的分叉。
#[derive(Clone, Debug, PartialEq)]
pub struct CoercionDecision {
    /// 路径标签。
    pub path: CoercionPath,
    /// 归一值（院内口径）；`None` 表示**查不到合法路径，不产出数值**。
    pub value_si: Option<f64>,
    /// 目标单位码（长度 `mm` / 容量 `mL`）；`None` 表示查不到路径。
    pub unit_code: Option<String>,
    /// 实际走过的步骤（空 = 没走成任何一步 = 查不到路径）。
    pub steps: Vec<CoercionStep>,
    /// 查不到路径的原因（`path == None` 时有值）。
    pub failure: Option<CoercionFailure>,
}

impl CoercionDecision {
    /// 构造一次**失败判定**（查不到合法路径，不产出数值）。
    pub fn failed(failure: CoercionFailure) -> CoercionDecision {
        CoercionDecision {
            path: CoercionPath::None,
            value_si: None,
            unit_code: None,
            steps: Vec::new(),
            failure: Some(failure),
        }
    }

    /// 是否成功（查到了合法路径并产出了归一值）。
    pub fn is_ok(&self) -> bool {
        !self.path.is_failure() && self.value_si.is_some()
    }

    /// **路径标签与步骤链是否自洽**（内核不变量）。
    ///
    /// | 路径 | 步骤链 |
    /// |---|---|
    /// | `UnitConvert` | 恰好一步 `UnitConvert` |
    /// | `DirectScale` | `Scale` 在前、`UnitConvert` 在后 |
    /// | `Composite` | 以 `DimensionalReduce` 开头、以 `UnitConvert` 结尾 |
    /// | `None` | 空步骤链、无归一值、无单位码 |
    pub fn is_consistent(&self) -> bool {
        let mut steps = self.steps.iter().copied();
        match self.path {
            CoercionPath::UnitConvert => {
                self.steps == [CoercionStep::UnitConvert]
            }
            CoercionPath::DirectScale => {
                steps.next() == Some(CoercionStep::Scale)
                    && steps.next() == Some(CoercionStep::UnitConvert)
                    && steps.next().is_none()
            }
            CoercionPath::Composite => {
                self.steps.first() == Some(&CoercionStep::DimensionalReduce)
                    && self.steps.last() == Some(&CoercionStep::UnitConvert)
            }
            CoercionPath::None => {
                self.steps.is_empty() && self.value_si.is_none() && self.unit_code.is_none()
            }
        }
    }

    /// 步骤链的可读形式（`DimensionalReduce → Scale → UnitConvert`）。
    pub fn step_chain(&self) -> String {
        self.steps
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" → ")
    }

    /// 一行人可读说明（失败清单与运行摘要用）。
    pub fn describe(&self) -> String {
        match (&self.value_si, &self.unit_code) {
            (Some(v), Some(c)) => {
                format!("{} → {} {}（{}）", self.path.as_str(), v, c, self.step_chain())
            }
            _ => match self.failure {
                Some(reason) => format!("{}（{}）", self.path.as_str(), reason.message()),
                None => self.path.as_str().to_string(),
            },
        }
    }
}

/// **判定内核**：一次判定给出路径、归一值、单位码与步骤链。
///
/// - `value` —— 报告原值的数值部分（**不含倍率**）。
/// - `scale` —— 原值词面自带的倍率（`3.2×2` 的 `2`）；没有倍率时为 `None`。
/// - `unit_raw` —— 报告原单位词面（`cm` / `厘米` / `CM` / `cm³` …）。
///
/// **fail-closed**：查不到合法路径一律返回 `path = None` 且**不产出数值**，
/// 不按任何单位兜底、不猜倍率。
pub fn decide(value: f64, scale: Option<f64>, unit_raw: &str) -> CoercionDecision {
    // 1. 数值必须有限 —— NaN / inf 不是测量值。
    if !value.is_finite() {
        return CoercionDecision::failed(CoercionFailure::ValueInvalid);
    }
    // 2. 倍率必须合法（有限且为正）—— 倍率是原文写明的，读不出/不合法就没有路径。
    let scale = match scale {
        None => None,
        Some(s) if s.is_finite() && s > 0.0 => Some(s),
        Some(_) => return CoercionDecision::failed(CoercionFailure::ScaleInvalid),
    };
    // 3. 单位必须查得到目录 —— 查不到就是查不到，不按任何单位兜底。
    let spec = match canonical_unit(unit_raw) {
        Some(spec) => spec,
        None => return CoercionDecision::failed(CoercionFailure::UnitUnknown),
    };

    // 4. 组装步骤链：量纲还原 → 倍率折算 → 单位换算。
    let mut steps: Vec<CoercionStep> = Vec::with_capacity(3);
    if spec.compound {
        steps.push(CoercionStep::DimensionalReduce);
    }
    if scale.is_some() {
        steps.push(CoercionStep::Scale);
    }
    steps.push(CoercionStep::UnitConvert);

    // 路径标签：**复合量纲优先于倍率**（量纲还原是换算的先决条件）。
    let path = if spec.compound {
        CoercionPath::Composite
    } else if scale.is_some() {
        CoercionPath::DirectScale
    } else {
        CoercionPath::UnitConvert
    };

    // 归一值：先乘倍率（原文写明的部分），再走**单位目录的换算**。
    //
    // **判定与换算同源**：算术不在判定里重写一遍，交给
    // [`super::units::convert_to_target`] —— 单位族内查换算表、值域规则
    // （负值 / 非有限 / 超量级）、3 位小数精度都在那一层，**判定与换算
    // 不会分叉**（不会出现「判了一条路、换算走另一条」）。
    let scaled = value * scale.unwrap_or(1.0);
    let (value_si, target) = match convert_to_target(scaled, spec.code) {
        Some((v, t)) => (v, t),
        // 换算层判无路径（负值 / 非有限 / 超量级）—— 不产出数值。
        None => return CoercionDecision::failed(CoercionFailure::ValueInvalid),
    };

    CoercionDecision {
        path,
        value_si: Some(value_si),
        unit_code: Some(target.to_string()),
        steps,
        failure: None,
    }
}

/// 判定一条测量值（**纯数值**）走哪条强转路径。
///
/// 数值不带倍率时用这个入口；带倍率走 [`decide`]（或 [`coerce_raw`]）。
pub fn find_coercion_path(value: f64, unit: &str) -> CoercionPath {
    decide(value, None, unit).path
}

/// 判定一条**原值词面**（可能自带倍率）走哪条强转路径。
///
/// `value_raw` 是**报告原值词面**（`3.2` / `3.2×2`），不是先 parse 过的数值。
pub fn find_coercion_path_raw(value_raw: &str, unit_raw: &str) -> CoercionPath {
    coerce_raw(value_raw, unit_raw).path
}

/// 按强转路径产出归一值与目标单位码。
///
/// 返回 `None` 表示查不到合法路径 —— 调用方据此产出失败条目，**不产出数值**。
pub fn coerce(value: f64, unit: &str) -> Option<(f64, String)> {
    coerce_scaled(value, None, unit)
}

/// 按强转路径产出归一值与目标单位码（**含倍率**）。
///
/// `scale` = 原值词面自带的倍率（`3.2×2` 的 `2`）；倍率先乘在原值上，再走单位换算。
pub fn coerce_scaled(value: f64, scale: Option<f64>, unit: &str) -> Option<(f64, String)> {
    let decision = decide(value, scale, unit);
    match (decision.value_si, decision.unit_code) {
        (Some(v), Some(code)) => Some((v, code)),
        _ => None,
    }
}

/// 从**报告原值词面**驱动一次完整判定。
///
/// 这是强转路径对照表（Task 23）的驱动入口：
/// `value_raw = "3.2×2"`、`unit_raw = "cm"` → `DirectScale` / `64` / `mm`。
///
/// 原值词面读不出数值、或倍率读不出来 → 查不到合法路径（**不猜倍率**）。
pub fn coerce_raw(value_raw: &str, unit_raw: &str) -> CoercionDecision {
    match parse_raw_value(value_raw) {
        Ok((value, scale)) => decide(value, scale, unit_raw),
        Err(failure) => CoercionDecision::failed(failure),
    }
}

/// 拆**原值词面** → （数值部分, 倍率）。
///
/// 倍率写法是原文的一部分（本表约定的唯一写法是 `<数值>×<倍数>`），
/// 因此倍率**只从词面里读**，不靠猜、不靠上下文推断。
pub fn parse_raw_value(raw: &str) -> Result<(f64, Option<f64>), CoercionFailure> {
    match split_scale_literal(raw) {
        RawScale::None => {
            let value = parse_number_literal(raw).ok_or(CoercionFailure::ValueInvalid)?;
            Ok((value, None))
        }
        RawScale::Some(tail) => {
            let value = parse_number_literal(head_of(raw)).ok_or(CoercionFailure::ValueInvalid)?;
            // 倍率读不出来 = 没有合法路径 —— 不猜倍率、不按 1 处理。
            let scale = parse_number_literal(tail).ok_or(CoercionFailure::ScaleUnreadable)?;
            Ok((value, Some(scale)))
        }
    }
}

/// 原值词面里的倍率部分。
enum RawScale<'a> {
    /// 词面里没有倍率。
    None,
    /// 词面里有倍率，倍率文本在此（可能读不出来）。
    Some(&'a str),
}

/// 在原值词面里找倍率符号（`×` / `x` / `X`）。
fn split_scale_literal(raw: &str) -> RawScale<'_> {
    for (i, c) in raw.char_indices() {
        if is_scale_sign(c) {
            let tail = raw[i + c.len_utf8()..].trim();
            return if tail.is_empty() {
                // `3.2×` —— 倍率链断了：按「倍率读不出来」处理，不当没有倍率。
                RawScale::Some("")
            } else {
                RawScale::Some(tail)
            };
        }
    }
    RawScale::None
}

/// 倍率符号（乘号 `×` 与 ASCII 字母 `x`）。
fn is_scale_sign(c: char) -> bool {
    c == '×' || c == 'x' || c == 'X'
}

/// 原值词面里倍率符号**之前**的部分。
fn head_of(raw: &str) -> &str {
    for (i, c) in raw.char_indices() {
        if is_scale_sign(c) {
            return raw[..i].trim();
        }
    }
    raw.trim()
}

/// 严格的数值字面量解析（只认数字与至多一个小数点）。
///
/// 不用 `f64::from_str` 的宽松形式：`"3.2.5"`、`"1e5"`、`"inf"` 都不该被当成测值。
fn parse_number_literal(raw: &str) -> Option<f64> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let bytes = t.as_bytes();
    let mut i = 0;
    let mut digits = 0usize;
    let mut dots = 0usize;
    if bytes[0] == b'-' {
        i = 1;
    }
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                digits += 1;
                i += 1;
            }
            b'.' => {
                dots += 1;
                i += 1;
            }
            _ => return None,
        }
    }
    if digits == 0 || dots > 1 {
        return None;
    }
    t.parse::<f64>().ok()
}

/// 单位族是否可直接互比（跨族返回 `false`）。
pub fn comparable(from: &str, to: &str) -> bool {
    match (canonical_unit(from), canonical_unit(to)) {
        (Some(a), Some(b)) => a.family == b.family,
        _ => false,
    }
}

/// 单位族（供上层按族选择目标口径）。
pub fn family_of(unit: &str) -> Option<Family> {
    canonical_unit(unit).map(|s| s.family)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 四类路径：各一条基准 ----

    #[test]
    fn 厘米转毫米走单位换算() {
        let d = decide(3.2, None, "cm");
        assert_eq!(d.path, CoercionPath::UnitConvert);
        assert_eq!(d.value_si, Some(32.0));
        assert_eq!(d.unit_code.as_deref(), Some("mm"));
        assert_eq!(d.steps, vec![CoercionStep::UnitConvert]);
        assert!(d.is_ok());
        assert!(d.is_consistent());
    }

    #[test]
    fn 未知单位判无路径且不产出数值() {
        let d = decide(3.2, None, "xx");
        assert_eq!(d.path, CoercionPath::None);
        assert_eq!(d.value_si, None);
        assert_eq!(d.unit_code, None);
        assert!(d.steps.is_empty());
        assert_eq!(d.failure, Some(CoercionFailure::UnitUnknown));
        assert!(!d.is_ok());
        assert!(d.is_consistent());
        // 旧入口同样判无路径。
        assert_eq!(find_coercion_path(3.2, "xx"), CoercionPath::None);
        assert!(coerce(3.2, "xx").is_none());
    }

    #[test]
    fn 复合量纲走两步() {
        let d = decide(3.2, None, "cm3");
        assert_eq!(d.path, CoercionPath::Composite);
        assert_eq!(d.value_si, Some(3.2));
        assert_eq!(d.unit_code.as_deref(), Some("mL"));
        assert_eq!(
            d.steps,
            vec![CoercionStep::DimensionalReduce, CoercionStep::UnitConvert]
        );
        assert!(d.is_consistent());
    }

    #[test]
    fn 复合量纲书写形式归一到同一条路径() {
        // `cm³` / `cm^3` / `㎤` 三种写法都指向同一条路径。
        for raw in ["cm3", "cm³", "cm^3", "㎤"] {
            let d = decide(3.2, None, raw);
            assert_eq!(d.path, CoercionPath::Composite, "{} 应走复合量纲", raw);
            assert_eq!(d.value_si, Some(3.2));
            assert_eq!(d.unit_code.as_deref(), Some("mL"));
        }
    }

    #[test]
    fn 带倍率走倍率路径且数值先乘倍率() {
        let d = decide(3.2, Some(2.0), "cm");
        assert_eq!(d.path, CoercionPath::DirectScale);
        assert_eq!(d.value_si, Some(64.0));
        assert_eq!(d.unit_code.as_deref(), Some("mm"));
        assert_eq!(
            d.steps,
            vec![CoercionStep::Scale, CoercionStep::UnitConvert]
        );
        assert!(d.is_consistent());
    }

    #[test]
    fn 复合量纲优先于倍率() {
        // 量纲还原是换算的先决条件 → 路径标签取 Composite，倍率体现在步骤链里。
        let d = decide(3.2, Some(2.0), "cm3");
        assert_eq!(d.path, CoercionPath::Composite);
        assert_eq!(d.value_si, Some(6.4));
        assert_eq!(
            d.steps,
            vec![
                CoercionStep::DimensionalReduce,
                CoercionStep::Scale,
                CoercionStep::UnitConvert
            ]
        );
        assert!(d.is_consistent());
    }

    // ---- 枚举往返 ----

    #[test]
    fn 路径字符串往返() {
        for p in CoercionPath::ALL {
            assert_eq!(CoercionPath::from_code(p.as_str()), Some(p));
            assert!(!p.label_zh().is_empty());
        }
        assert_eq!(CoercionPath::from_code("Unknown"), None);
        assert_eq!(CoercionPath::from_code(""), None);
        // 失败路径只有一个。
        assert_eq!(
            CoercionPath::ALL
                .iter()
                .filter(|p| p.is_failure())
                .count(),
            1
        );
    }

    // ---- fail-closed：查不到路径就不产出数值 ----

    #[test]
    fn 非有限数值查不到路径() {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let d = decide(v, None, "cm");
            assert_eq!(d.path, CoercionPath::None);
            assert_eq!(d.value_si, None);
            assert_eq!(d.failure, Some(CoercionFailure::ValueInvalid));
        }
    }

    #[test]
    fn 非法倍率查不到路径且不猜倍率() {
        for s in [0.0, -2.0, f64::NAN, f64::INFINITY] {
            let d = decide(3.2, Some(s), "cm");
            assert_eq!(d.path, CoercionPath::None, "倍率 {} 应判无路径", s);
            assert_eq!(d.value_si, None);
            assert_eq!(d.failure, Some(CoercionFailure::ScaleInvalid));
        }
    }

    #[test]
    fn 负值查不到路径不产出数值() {
        // 长度/容量不会为负 —— 负数留在台账里会像一条正常测量值。
        for v in [-3.2, -0.001, -1.0e9] {
            let d = decide(v, None, "cm");
            assert_eq!(d.path, CoercionPath::None, "负值 {} 应判无路径", v);
            assert_eq!(d.value_si, None);
            assert_eq!(d.failure, Some(CoercionFailure::ValueInvalid));
        }
        // 倍率非负也不救：原值本身是负的就没有合法路径。
        let d = decide(-3.2, Some(2.0), "cm");
        assert_eq!(d.path, CoercionPath::None);
        assert_eq!(d.value_si, None);
    }

    #[test]
    fn 零值是合法测量值照常判路径() {
        // 残余尿量 0 mL、积水 0 cm 都是真实结果。
        let d = decide(0.0, None, "cm");
        assert_eq!(d.path, CoercionPath::UnitConvert);
        assert_eq!(d.value_si, Some(0.0));
        assert_eq!(d.unit_code.as_deref(), Some("mm"));
    }

    #[test]
    fn 超量级查不到路径不产出数值() {
        // 3 位小数口径下该量级已超出 f64 有效位数。
        let over = crate::coerce::units::MAX_MEASUREMENT_MAGNITUDE * 10.0;
        let d = decide(over, None, "cm");
        assert_eq!(d.path, CoercionPath::None);
        assert_eq!(d.value_si, None);
        assert_eq!(d.failure, Some(CoercionFailure::ValueInvalid));
    }

    #[test]
    fn 未知单位不按任何单位兜底() {
        // `cc` 不按 `cm`、`mm2` 不按 `mm`、`g`（质量族）不按长度 —— 一律查不到。
        for raw in ["cc", "mm2", "cm2", "g", "in", "dl", "um", "cmx"] {
            let d = decide(2.8, None, raw);
            assert_eq!(d.path, CoercionPath::None, "{} 不应有合法路径", raw);
            assert_eq!(d.value_si, None);
            assert_eq!(d.failure, Some(CoercionFailure::UnitUnknown));
        }
    }

    #[test]
    fn 缺单位查不到路径() {
        let d = decide(3.2, None, "");
        assert_eq!(d.path, CoercionPath::None);
        assert_eq!(d.failure, Some(CoercionFailure::UnitUnknown));
    }

    // ---- 单位归一（大小写 / 中文名） ----

    #[test]
    fn 中文单位名与大小写归一到同一路径() {
        for raw in ["厘米", "CM", "Cm", "cm"] {
            let d = decide(3.2, None, raw);
            assert_eq!(d.path, CoercionPath::UnitConvert, "{} 应走单位换算", raw);
            assert_eq!(d.value_si, Some(32.0));
            assert_eq!(d.unit_code.as_deref(), Some("mm"));
        }
        for raw in ["毫升", "ml", "ML", "mL"] {
            let d = decide(120.0, None, raw);
            assert_eq!(d.path, CoercionPath::UnitConvert);
            assert_eq!(d.value_si, Some(120.0));
            assert_eq!(d.unit_code.as_deref(), Some("mL"));
        }
    }

    #[test]
    fn 目标口径即源单位时换算因子为一() {
        // `85mm` 走的是同族的「换算」，因子为 1 —— 路径标签仍是 UnitConvert。
        let d = decide(85.0, None, "mm");
        assert_eq!(d.path, CoercionPath::UnitConvert);
        assert_eq!(d.value_si, Some(85.0));
        assert_eq!(d.unit_code.as_deref(), Some("mm"));
    }

    // ---- 原值词面驱动（对照表入口） ----

    #[test]
    fn 原值词面自带倍率判倍率路径() {
        let d = coerce_raw("3.2×2", "cm");
        assert_eq!(d.path, CoercionPath::DirectScale);
        assert_eq!(d.value_si, Some(64.0));
        assert_eq!(d.unit_code.as_deref(), Some("mm"));
        assert_eq!(find_coercion_path_raw("3.2×2", "cm"), CoercionPath::DirectScale);
    }

    #[test]
    fn 原值词面不含倍率判单位换算路径() {
        let d = coerce_raw("3.2", "cm");
        assert_eq!(d.path, CoercionPath::UnitConvert);
        assert_eq!(d.value_si, Some(32.0));
        assert_eq!(d.unit_code.as_deref(), Some("mm"));
    }

    #[test]
    fn 原值词面倍率读不出来判无路径() {
        // `3.2×` 链断了、`3.2×abc` 读不出倍率 —— 都不猜倍率。
        for raw in ["3.2×", "3.2×abc", "3.2× "] {
            let d = coerce_raw(raw, "cm");
            assert_eq!(d.path, CoercionPath::None, "{} 不应有合法路径", raw);
            assert_eq!(d.value_si, None);
            assert_eq!(d.failure, Some(CoercionFailure::ScaleUnreadable));
        }
    }

    #[test]
    fn 原值词面不是数值判无路径() {
        for raw in ["", "  ", "abc", "3.2.5", "1e5", "inf", "NaN"] {
            let d = coerce_raw(raw, "cm");
            assert_eq!(d.path, CoercionPath::None, "{} 不应有合法路径", raw);
            assert_eq!(d.value_si, None);
            assert_eq!(d.failure, Some(CoercionFailure::ValueInvalid));
        }
    }

    #[test]
    fn 原值词面前置小数点按数值解释() {
        let d = coerce_raw(".5", "cm");
        assert_eq!(d.path, CoercionPath::UnitConvert);
        assert_eq!(d.value_si, Some(5.0));
    }

    // ---- 内不变量：路径标签与步骤链自洽 ----

    #[test]
    fn 路径与步骤链始终自洽() {
        let cases: Vec<(f64, Option<f64>, &str)> = vec![
            (3.2, None, "cm"),
            (3.2, None, "mm"),
            (3.2, Some(2.0), "cm"),
            (3.2, Some(0.5), "cm"),
            (3.2, None, "cm3"),
            (3.2, Some(2.0), "cm3"),
            (1.0, None, "L"),
            (3.2, None, "xx"),
            (3.2, Some(0.0), "cm"),
            (f64::NAN, None, "cm"),
        ];
        for (v, s, u) in cases {
            let d = decide(v, s, u);
            assert!(
                d.is_consistent(),
                "判定不自洽：value={} scale={:?} unit={} → {:?}",
                v,
                s,
                u,
                d
            );
        }
    }

    #[test]
    fn 成功判定必有归一值与单位码失败判定必无() {
        let ok = decide(3.2, None, "cm");
        assert!(ok.value_si.is_some() && ok.unit_code.is_some());
        let bad = decide(3.2, None, "xx");
        assert!(bad.value_si.is_none() && bad.unit_code.is_none());
        assert!(bad.failure.is_some());
        assert!(ok.failure.is_none());
    }

    #[test]
    fn 判定说明可读() {
        let ok = decide(3.2, None, "cm");
        assert_eq!(ok.describe(), "UnitConvert → 32 mm（UnitConvert）");
        let bad = decide(3.2, None, "xx");
        assert!(bad.describe().contains("单位不在本轮口径目录内"));
    }

    // ---- 族内可比 ----

    #[test]
    fn 跨族不可比() {
        assert!(!comparable("cm", "mL"));
        assert!(comparable("cm", "mm"));
        assert!(!comparable("cm", "xx"));
        assert_eq!(family_of("cm"), Some(Family::Length));
        assert_eq!(family_of("mL"), Some(Family::Volume));
        assert_eq!(family_of("xx"), None);
    }

    // ---- 旧入口仍在（回归别的调用方） ----

    #[test]
    fn 旧入口与新判定同源() {
        assert_eq!(find_coercion_path(3.2, "cm"), CoercionPath::UnitConvert);
        assert_eq!(find_coercion_path(3.2, "cm3"), CoercionPath::Composite);
        assert_eq!(coerce(1.0, "L"), Some((1000.0, "mL".to_string())));
        assert_eq!(coerce_scaled(3.2, Some(2.0), "cm"), Some((64.0, "mm".to_string())));
    }
}
