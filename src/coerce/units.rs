//! 强转内核：单位目录与换算表。
//!
//! 单位码取 **UCUM**（`mm` / `cm` / `mL` / `L`）；中文单位名与 UCUM 码可往返
//! （大小写不敏感），未知单位一律拒绝，**不回落默认单位**。
//!
//! 口径说明：长度族目标口径为 **mm**、容量族目标口径为 **mL**（院内口径），
//! 换算因子是**经验约定的院内口径**，不是法定计量标准。
//!
//! **每条换算都带来源说明**：单位码从哪份公开码表来、换算因子凭什么这么定 ——
//! 写在 [`UnitSpec::source`] 与 [`Conversion::source`] 里，口径追溯时不必去猜。
//! 码表来源与 [`crate::publicdata`] 取回的 `assets/public/ucum/unit_crosswalk.csv`
//! 逐条对得上：`mm` / `cm` / `m` / `mL` 取自 **UCUM 完整码表**，`L` 取自**官方策展
//! essential 子集**，`cm3` 两份发布包**均未收录**（本项目的简化写法），换算按院内约定。
//!
//! **词面归一（`normalize_unit_spelling`）**：报告里同一单位的写法不统一 ——
//! `㎝` / `cm³` / `cm^3` / `CM` 指的是同一件事。归一在**目录这一层**做，
//! 词法层只负责把单位词面**整块切出来**（含指数），两层职责不重叠：
//! 词法层回答「这个词面到哪结束」，目录回答「这个词面是什么单位」。
//!
//! **名字层在 [`super::ucum`]**：UCUM 码 ↔ 中文单位名的双向映射（大小写不敏感、
//! 未知即拒绝）只有一份实现 —— 本模块的 [`canonical_unit`] 经它认单位，
//! 两处各写一份必然漂移（漂移表现为「目录认得、名字层不认」或反之）。
//!
//! # 换算表
//!
//! [`conversion_table`] 是**族内全部有序对** —— 由单位目录派生，
//! **不可能与目录漂移**（改一条目录项，表跟着变）。每条换算带**因子**与**来源说明**：
//!
//! | 换算 | 因子 | 来源（摘要） |
//! |---|---|---|
//! | `cm → mm` | 10 | UCUM 完整码表 `centimeter` → `millimeter`；1 cm = 10 mm（经验约定的院内口径） |
//! | `L → mL` | 1000 | UCUM 官方策展 essential 子集 `liter` → 完整码表 `milliliter`；1 L = 1000 mL（经验约定的院内口径） |
//! | `cm3 → mL` | 1 | UCUM 未收录 `cm³`（本项目简化写法）→ 完整码表 `milliliter`；1 cm³ = 1 mL（经验约定的院内口径） |
//!
//! **跨族不在表内** —— `cm → mL` 查不到就是查不到：长度与容量之间没有换算，
//! 硬换出来的数字是编的。
//!
//! # 值域规则（[`is_measurement_value`]）
//!
//! | 情况 | 处置 | 为什么 |
//! |---|---|---|
//! | 有限、非负、不超量级 | 换算 | 正常测量值 |
//! | **零值** | 换算（结果为 0） | 残余尿量 0 mL、积水 0 cm 是**真实结果**，不是坏值 |
//! | **负值** | 判无路径，**不产出数值** | 长度/容量不会为负；负数是报告侧写错，留在台账里会像一条正常测量值 |
//! | **超量级**（> [`MAX_MEASUREMENT_MAGNITUDE`]） | 判无路径，**不产出数值** | 3 位小数口径下该量级已超出 f64 有效位数，再换算只会得到「看着精确、实则已失真」的数字 |
//! | 非有限（`NaN` / `inf`） | 判无路径，**不产出数值** | 不是测量值 |
//!
//! > **「超长数值」的两层处置**：**字面量过长**（几十位数字串）在解析层判
//! > `VALUE_TOO_LONG`（见 [`crate::parse::extract_values`]）；**量级超上限**在本层
//! > 判无路径。两层各管一段，别混。
//!
//! **精度**：换算结果保留 **3 位小数**（[`round3`]）—— 与判定内核
//! （[`crate::coerce::path`]）**同源**，不存在「判了一条路、换算走另一条」。

use std::sync::OnceLock;

use super::ucum;

/// **名字层的公开出口**（词法层按 `crate::coerce::units::…` 引用，路径保持不变）。
///
/// 实现在 [`super::ucum`]：UCUM 码 ↔ 中文单位名 的双向映射与书写形式归一。
pub use super::ucum::{
    compat_unit, is_fullwidth_latin, normalize_unit_spelling, superscript_digit,
    to_halfwidth_latin,
};

/// 单位族。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// 长度族（目标 mm）。
    Length,
    /// 容量族（目标 mL）。
    Volume,
}

/// 一条单位目录项。
#[derive(Clone, Copy, Debug)]
pub struct UnitSpec {
    /// UCUM 码（大小写敏感，按 UCUM 原样）。
    pub code: &'static str,
    /// 单位族。
    pub family: Family,
    /// 换算到本族目标口径的乘数（`value_si = value_raw × factor`）。
    pub factor: f64,
    /// 是否复合量纲（cm³ 这类需要「量纲还原 + 单位换算」两步）。
    pub compound: bool,
    /// **来源说明**：单位码取自哪份公开码表、换算因子凭什么这么定。
    pub source: &'static str,
}

/// 一条**换算项**：族内某一对单位的换算（因子 + 来源说明）。
#[derive(Clone, Debug, PartialEq)]
pub struct Conversion {
    /// 源单位 UCUM 码。
    pub from: &'static str,
    /// 目标单位 UCUM 码。
    pub to: &'static str,
    /// 换算因子（`value_to = value_from × factor`）。
    pub factor: f64,
    /// **来源说明**：两端单位的码表来源 + 因子依据 + 院内口径声明。
    pub source: String,
}

/// 本轮口径允许的**测量值量级上限**。
///
/// 3 位小数口径要求目标值在 15 位有效数字内可表示；`1e12` 再加 3 位小数就到
/// 16 位，**超出 f64 的有效位数（约 15–16 位）** —— 这个量级上的「精确到毫米」
/// 是假的。宁可判无路径让医师重写，也不要把失真数字写进台账。
pub const MAX_MEASUREMENT_MAGNITUDE: f64 = 1e12;

/// 目标口径单位码。
pub fn target_code(family: Family) -> &'static str {
    match family {
        Family::Length => "mm",
        Family::Volume => "mL",
    }
}

/// 单位目录：UCUM 码 → 目录项。
///
/// `source` 逐条写清**码表来源**与**因子依据** —— 与
/// `assets/public/ucum/unit_crosswalk.csv` 的 `note` 列一一对应。
const UNITS: &[UnitSpec] = &[
    UnitSpec {
        code: "mm",
        family: Family::Length,
        factor: 1.0,
        compound: false,
        source: "UCUM 完整码表 millimeter；1 mm = 1 mm（目标口径即 mm，经验约定的院内口径）",
    },
    UnitSpec {
        code: "cm",
        family: Family::Length,
        factor: 10.0,
        compound: false,
        source: "UCUM 完整码表 centimeter；1 cm = 10 mm（经验约定的院内口径）",
    },
    UnitSpec {
        code: "m",
        family: Family::Length,
        factor: 1000.0,
        compound: false,
        source: "UCUM 完整码表 meter；1 m = 1000 mm（经验约定的院内口径）",
    },
    UnitSpec {
        code: "mL",
        family: Family::Volume,
        factor: 1.0,
        compound: false,
        source: "UCUM 完整码表 milliliter；1 mL = 1 mL（目标口径即 mL，经验约定的院内口径）",
    },
    UnitSpec {
        code: "L",
        family: Family::Volume,
        factor: 1000.0,
        compound: false,
        source: "UCUM 官方策展 essential 子集 liter；1 L = 1000 mL（经验约定的院内口径）",
    },
    // 复合量纲：1 cm³ = 1 mL，需两步（量纲还原 + 单位换算）。
    UnitSpec {
        code: "cm3",
        family: Family::Volume,
        factor: 1.0,
        compound: true,
        source: "UCUM 两份发布包均未收录，本项目为简化写法；1 cm³ = 1 mL（经验约定的院内口径）",
    },
];

/// 把原单位词面归一到单位目录项。
///
/// 认定「这是什么单位」交给**名字层**（[`super::ucum`]）：拉丁字母大小写不敏感
/// （`CM` / `cm` / `Cm` 同义；`ml` / `mL` / `ML` 同义）、中文单位名与别名按
/// 目录归一、书写形式先归一（`㎝` / `cm³` / `cm^3` → `cm3`）。
///
/// 未知单位返回 `None` —— **不默认按某单位处理**。
pub fn canonical_unit(raw: &str) -> Option<UnitSpec> {
    let code = ucum::canonical_code(raw)?;
    lookup_code(code)
}

/// 按 UCUM 码查目录项（**大小写敏感**，按 UCUM 原样）。
pub fn lookup_code(code: &str) -> Option<UnitSpec> {
    UNITS.iter().find(|s| s.code == code).copied()
}

/// 全部单位目录项（族属 + 换算因子 + 来源说明）。
///
/// 与 [`super::ucum::entries`]（名字层）的**码集合必须相等** ——
/// 由 `tests/coerce_ucum.rs` 的跨模块断言守住。
pub fn catalog() -> &'static [UnitSpec] {
    UNITS
}

/// **换算表**：族内全部有序对（由单位目录派生，**不与目录漂移**）。
///
/// 跨族不在表内 —— 表里有就是有合法换算，没有就是没有。
static CONVERSIONS: OnceLock<Vec<Conversion>> = OnceLock::new();

/// 换算表（族内全部有序对）。
pub fn conversion_table() -> &'static [Conversion] {
    CONVERSIONS.get_or_init(build_conversions)
}

/// 由单位目录派生换算表：同族两两配对，因子 = 源因子 ÷ 目标因子。
fn build_conversions() -> Vec<Conversion> {
    let mut out = Vec::new();
    for from in UNITS {
        for to in UNITS {
            if from.family != to.family {
                // 跨族不进表：长度与容量之间没有换算。
                continue;
            }
            let factor = from.factor / to.factor;
            out.push(Conversion {
                from: from.code,
                to: to.code,
                factor,
                source: format!(
                    "{} → {}：{}；{}；换算因子 {}（经验约定的院内口径，非法定计量标准）",
                    from.code, to.code, from.source, to.source, factor
                ),
            });
        }
    }
    out
}

/// 查换算表里的因子（**跨族 / 未知单位返回 `None`**）。
fn conversion_factor(from_code: &str, to_code: &str) -> Option<f64> {
    conversion_table()
        .iter()
        .find(|c| c.from == from_code && c.to == to_code)
        .map(|c| c.factor)
}

/// 某一对单位的**换算来源说明**（跨族 / 未知单位返回 `None`）。
///
/// 口径追溯用：拿到一条台账行，能一路查到「这个因子凭什么这么定」。
pub fn conversion_source(from: &str, to: &str) -> Option<String> {
    let from_spec = canonical_unit(from)?;
    let to_spec = canonical_unit(to)?;
    conversion_table()
        .iter()
        .find(|c| c.from == from_spec.code && c.to == to_spec.code)
        .map(|c| c.source.clone())
}

/// **值域规则**：测量值须有限、非负、不超量级。
///
/// - 零值**合格**（残余尿量 0 mL 是真实结果）。
/// - 负值**不合格**（长度/容量不会为负 —— 那是报告侧写错）。
/// - 超量级**不合格**（3 位小数口径下已超出 f64 有效位数）。
pub fn is_measurement_value(value: f64) -> bool {
    // 区间判定同时挡住 `NaN` 与 `±inf`（含端点比较对 `NaN` 恒为 false）。
    (0.0..=MAX_MEASUREMENT_MAGNITUDE).contains(&value)
}

/// 保留 3 位小数（院内口径精度）。
pub fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// **族内换算**：同族换算成功，跨族 / 未知单位返回 `None`。
///
/// ```text
/// convert(3.2, "cm", "mm") == Some(32.0)
/// convert(1.0, "L", "mL")  == Some(1000.0)
/// convert(3.2, "cm", "mL") == None   // 跨族：没有这条换算
/// convert(3.2, "xx", "mm") == None   // 未知单位
/// convert(-3.2, "cm", "mm") == None  // 负值不是测量值
/// ```
///
/// **fail-closed**：查不到合法换算一律返回 `None`，**不按任何单位兜底**。
/// 结果保留 3 位小数。
pub fn convert(value: f64, from: &str, to: &str) -> Option<f64> {
    let from_spec = canonical_unit(from)?;
    let to_spec = canonical_unit(to)?;
    if from_spec.family != to_spec.family {
        // 跨族：长度与容量之间没有换算 —— 不硬换。
        return None;
    }
    let factor = conversion_factor(from_spec.code, to_spec.code)?;
    if !is_measurement_value(value) {
        // 负值 / 非有限 / 超量级：不产出数值。
        return None;
    }
    let out = value * factor;
    if !is_measurement_value(out) {
        // 换算后超出量级上限（3 位小数口径已失真）—— 不产出数值。
        return None;
    }
    Some(round3(out))
}

/// 换算到**本族目标口径**（长度 → mm、容量 → mL），并给出目标单位码。
///
/// 判定内核（[`crate::coerce::path`]）的算术入口 —— 判定与换算**同源**。
pub fn convert_to_target(value: f64, unit: &str) -> Option<(f64, &'static str)> {
    let spec = canonical_unit(unit)?;
    let target = target_code(spec.family);
    let converted = convert(value, spec.code, target)?;
    Some((converted, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 目录层 ----

    #[test]
    fn 长度族换算因子正确() {
        let cm = canonical_unit("cm").unwrap();
        assert_eq!(cm.factor, 10.0);
        assert_eq!(cm.code, "cm");
        let mm = canonical_unit("mm").unwrap();
        assert_eq!(mm.factor, 1.0);
        assert_eq!(mm.code, "mm");
    }

    #[test]
    fn 容量族换算因子正确() {
        let l = canonical_unit("L").unwrap();
        assert_eq!(l.factor, 1000.0);
        assert_eq!(l.code, "L");
    }

    #[test]
    fn 大小写不敏感归一() {
        assert_eq!(canonical_unit("CM").unwrap().code, "cm");
        assert_eq!(canonical_unit("ML").unwrap().code, "mL");
        assert_eq!(canonical_unit("ml").unwrap().code, "mL");
    }

    #[test]
    fn 中文单位名归一到码() {
        assert_eq!(canonical_unit("厘米").unwrap().code, "cm");
        assert_eq!(canonical_unit("毫米").unwrap().code, "mm");
        assert_eq!(canonical_unit("毫升").unwrap().code, "mL");
        assert_eq!(canonical_unit("立方厘米").unwrap().code, "cm3");
    }

    #[test]
    fn 未知单位拒绝() {
        assert!(canonical_unit("xx").is_none());
        assert!(canonical_unit("").is_none());
    }

    #[test]
    fn 复合量纲书写形式归一到同一词面() {
        // 上标、幂写法、方块兼容字符 —— 三种写法都指向 cm³。
        assert_eq!(normalize_unit_spelling("cm³"), "cm3");
        assert_eq!(normalize_unit_spelling("cm^3"), "cm3");
        assert_eq!(normalize_unit_spelling("㎤"), "cm3");
        for raw in ["cm3", "cm³", "cm^3", "㎤"] {
            let spec = canonical_unit(raw).expect("复合量纲应能查到目录项");
            assert_eq!(spec.code, "cm3");
            assert!(spec.compound);
        }
    }

    #[test]
    fn 兼容方块单位归一() {
        assert_eq!(normalize_unit_spelling("㎝"), "cm");
        assert_eq!(normalize_unit_spelling("㎜"), "mm");
        assert_eq!(canonical_unit("㎝").unwrap().code, "cm");
        assert_eq!(canonical_unit("㎜").unwrap().code, "mm");
    }

    #[test]
    fn 全角拉丁单位归一() {
        assert_eq!(normalize_unit_spelling("ＣＭ"), "CM");
        assert_eq!(canonical_unit("ＣＭ").unwrap().code, "cm");
    }

    #[test]
    fn 面积量纲仍是未知单位() {
        // mm² / cm² 不在本轮口径内：归一后仍查不到目录 —— 不默认按长度处理。
        assert!(canonical_unit("mm2").is_none());
        assert!(canonical_unit("cm2").is_none());
        assert!(canonical_unit("㎟").is_none());
        assert!(canonical_unit("㎠").is_none());
    }

    // ---- 每条换算都带来源说明 ----

    #[test]
    fn 每条目录项都带来源说明() {
        for spec in UNITS {
            assert!(!spec.source.is_empty(), "{} 缺来源说明", spec.code);
            assert!(
                spec.source.contains("UCUM") || spec.source.contains("简化写法"),
                "{} 的来源说明没说清码表来源：{}",
                spec.code,
                spec.source
            );
        }
        // 逐条与公开码表核对清单对得上（`cm3` 是唯一未被 UCUM 收录的码）。
        assert!(canonical_unit("cm").unwrap().source.contains("完整码表"));
        assert!(canonical_unit("L").unwrap().source.contains("essential"));
        assert!(canonical_unit("cm3").unwrap().source.contains("未收录"));
    }

    #[test]
    fn 每条换算都带来源说明且说明两端码表来源() {
        for c in conversion_table() {
            assert!(!c.source.is_empty(), "{} → {} 缺来源说明", c.from, c.to);
            assert!(c.source.contains(c.from) && c.source.contains(c.to));
            assert!(c.source.contains("换算因子"));
        }
    }

    #[test]
    fn 换算表覆盖族内全部有序对且不含跨族() {
        let table = conversion_table();
        // 长度族 3 个单位 → 9 对；容量族 3 个单位 → 9 对。
        assert_eq!(table.len(), 18);
        for from in UNITS {
            for to in UNITS {
                let same_family = from.family == to.family;
                let hit = table
                    .iter()
                    .find(|c| c.from == from.code && c.to == to.code);
                assert_eq!(
                    hit.is_some(),
                    same_family,
                    "{} → {} 在表中的存在性应等于同族",
                    from.code,
                    to.code
                );
                if same_family {
                    let c = hit.expect("同族对应在表内");
                    assert_eq!(c.factor, from.factor / to.factor);
                }
            }
        }
    }

    #[test]
    fn 换算表来源说明可查() {
        let note = conversion_source("cm", "mm").expect("族内换算应有来源说明");
        assert!(note.contains("cm"));
        assert!(note.contains("mm"));
        assert!(note.contains("10"));
        // 跨族 / 未知单位查不到来源说明。
        assert!(conversion_source("cm", "mL").is_none());
        assert!(conversion_source("xx", "mm").is_none());
    }

    // ---- 换算：同族成功，跨族返回空 ----

    #[test]
    fn 升转毫升数值精确() {
        assert_eq!(convert(1.0, "L", "mL"), Some(1000.0));
        assert_eq!(convert(0.5, "L", "mL"), Some(500.0));
        assert_eq!(convert(1000.0, "mL", "L"), Some(1.0));
    }

    #[test]
    fn 厘米转毫米数值精确() {
        assert_eq!(convert(3.2, "cm", "mm"), Some(32.0));
        assert_eq!(convert(0.6, "cm", "mm"), Some(6.0));
        assert_eq!(convert(85.0, "mm", "mm"), Some(85.0));
        assert_eq!(convert(32.0, "mm", "cm"), Some(3.2));
    }

    #[test]
    fn 跨族换算返回空() {
        // 长度与容量之间没有换算 —— 硬换出来的数字是编的。
        assert_eq!(convert(3.2, "cm", "mL"), None);
        assert_eq!(convert(3.2, "mL", "cm"), None);
        assert_eq!(convert(1.0, "L", "mm"), None);
    }

    #[test]
    fn 未知单位换算返回空() {
        assert_eq!(convert(3.2, "xx", "mm"), None);
        assert_eq!(convert(3.2, "cm", "xx"), None);
        assert_eq!(convert(3.2, "", "mm"), None);
        // 面积量纲不在本轮口径内 —— 不默认按长度处理。
        assert_eq!(convert(3.2, "cm2", "mm"), None);
    }

    #[test]
    fn 中文单位名与大小写走同一条换算() {
        assert_eq!(convert(3.2, "厘米", "毫米"), Some(32.0));
        assert_eq!(convert(3.2, "CM", "mm"), Some(32.0));
        assert_eq!(convert(120.0, "毫升", "mL"), Some(120.0));
    }

    // ---- 边界：负值 / 零值 / 超量级 / 精度 ----

    #[test]
    fn 零值是合法测量值照常换算() {
        // 残余尿量 0 mL、积水 0 cm 都是真实结果 —— 不是坏值。
        assert_eq!(convert(0.0, "cm", "mm"), Some(0.0));
        assert_eq!(convert(0.0, "mL", "mL"), Some(0.0));
        assert!(is_measurement_value(0.0));
    }

    #[test]
    fn 负值不换算不产出数值() {
        // 长度/容量不会为负 —— 负数留在台账里会像一条正常测量值。
        for raw in [-3.2, -0.0001, -1.0e9] {
            assert_eq!(convert(raw, "cm", "mm"), None, "{} 不应产出数值", raw);
            assert!(!is_measurement_value(raw));
        }
    }

    #[test]
    fn 超量级不换算不产出数值() {
        // 3 位小数口径下该量级已超出 f64 有效位数。
        let over = MAX_MEASUREMENT_MAGNITUDE * 10.0;
        assert_eq!(convert(over, "cm", "mm"), None);
        assert!(!is_measurement_value(over));
        // 换算后才超上限（源值合格、结果失真）同样不产出数值。
        assert_eq!(convert(MAX_MEASUREMENT_MAGNITUDE, "cm", "mm"), None);
        assert!(is_measurement_value(MAX_MEASUREMENT_MAGNITUDE));
    }

    #[test]
    fn 非有限值不换算() {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(convert(v, "cm", "mm"), None);
            assert!(!is_measurement_value(v));
        }
    }

    #[test]
    fn 结果保留三位小数() {
        // 0.1 cm = 1 mm —— 精确值不该被四舍五入成 1.001。
        assert_eq!(convert(0.1, "cm", "mm"), Some(1.0));
        // 1/3 类值按 3 位小数落。
        assert_eq!(convert(1.0 / 3.0, "cm", "mm"), Some(3.333));
        assert_eq!(round3(1.23456), 1.235);
        assert_eq!(round3(1.2344), 1.234);
    }

    #[test]
    fn 换算到本族目标口径给出目标单位码() {
        assert_eq!(convert_to_target(3.2, "cm"), Some((32.0, "mm")));
        assert_eq!(convert_to_target(1.0, "L"), Some((1000.0, "mL")));
        assert_eq!(convert_to_target(3.2, "xx"), None);
    }
}
