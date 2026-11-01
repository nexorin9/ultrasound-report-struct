//! 强转内核第 2 层：**单位目录与换算表**的验收用例。
//!
//! 本文件守的是**口径**而不是实现细节：
//!
//! - 同族换算数值**精确**（`1 L → 1000 mL`、`3.2 cm → 32 mm`）；
//! - **跨族换算返回 `None`**（长度与容量之间没有换算）；
//! - 边界（**负值 / 零值 / 超量级 / 精度**）各有明确处置；
//! - 每条换算都**带来源说明**（口径追溯时不必去猜）。
//!
//! 判定内核（[`ultrasound_report_struct::coerce::path`]）与换算层**同源** ——
//! 本文件同时断言：判无路径的输入，换算层也拿不到数值。

use ultrasound_report_struct::coerce::path::{decide, CoercionFailure, CoercionPath};
use ultrasound_report_struct::coerce::units::{
    conversion_source, conversion_table, convert, convert_to_target, is_measurement_value,
    MAX_MEASUREMENT_MAGNITUDE,
};

#[test]
fn 族内换算数值精确() {
    // 容量族：1 L = 1000 mL。
    assert_eq!(convert(1.0, "L", "mL"), Some(1000.0));
    assert_eq!(convert(0.5, "L", "mL"), Some(500.0));
    assert_eq!(convert(1000.0, "mL", "L"), Some(1.0));
    // 长度族：3.2 cm = 32 mm。
    assert_eq!(convert(3.2, "cm", "mm"), Some(32.0));
    assert_eq!(convert(0.6, "cm", "mm"), Some(6.0));
    assert_eq!(convert(85.0, "mm", "mm"), Some(85.0));
    // 复合量纲：1 cm³ = 1 mL。
    assert_eq!(convert(3.2, "cm3", "mL"), Some(3.2));
}

#[test]
fn 跨族换算返回空不硬换() {
    // 长度与容量之间没有换算 —— 硬换出来的数字是编的。
    for (v, from, to) in [
        (3.2, "cm", "mL"),
        (3.2, "mL", "cm"),
        (1.0, "L", "mm"),
        (100.0, "mm", "L"),
    ] {
        assert_eq!(
            convert(v, from, to),
            None,
            "{from} → {to} 跨族，不该产出数值"
        );
    }
}

#[test]
fn 未知单位换算返回空() {
    for (from, to) in [("xx", "mm"), ("cm", "xx"), ("", "mm"), ("cm2", "mm"), ("cc", "mL")] {
        assert_eq!(convert(3.2, from, to), None, "{from} → {to} 不该产出数值");
    }
}

#[test]
fn 边界处置明确() {
    // 零值是合法测量值（残余尿量 0 mL）—— 照常换算。
    assert_eq!(convert(0.0, "cm", "mm"), Some(0.0));
    assert!(is_measurement_value(0.0));

    // 负值不是测量值 —— 不产出数值。
    for v in [-3.2, -0.001, -1.0e9] {
        assert_eq!(convert(v, "cm", "mm"), None, "{v} 不该产出数值");
        assert!(!is_measurement_value(v));
    }

    // 超量级：3 位小数口径下已超出 f64 有效位数 —— 不产出数值。
    let over = MAX_MEASUREMENT_MAGNITUDE * 10.0;
    assert_eq!(convert(over, "cm", "mm"), None);
    assert!(!is_measurement_value(over));
    // 源值合格但换算后超上限 —— 同样不产出数值。
    assert_eq!(convert(MAX_MEASUREMENT_MAGNITUDE, "cm", "mm"), None);

    // 非有限值不是测量值。
    for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(convert(v, "cm", "mm"), None);
        assert!(!is_measurement_value(v));
    }
}

#[test]
fn 精度保留三位小数() {
    assert_eq!(convert(0.1, "cm", "mm"), Some(1.0));
    assert_eq!(convert(1.0 / 3.0, "cm", "mm"), Some(3.333));
    assert_eq!(convert(2.0 / 3.0, "cm", "mm"), Some(6.667));
}

#[test]
fn 每条换算都带来源说明() {
    let table = conversion_table();
    assert!(!table.is_empty());
    for c in table {
        assert!(!c.source.is_empty(), "{} → {} 缺来源说明", c.from, c.to);
        assert!(c.source.contains("UCUM"), "{} → {} 来源没说清码表", c.from, c.to);
        assert!(c.source.contains("换算因子"));
    }
    // 跨族查不到来源说明（表里没有这一对）。
    assert!(conversion_source("cm", "mL").is_none());
    assert!(conversion_source("xx", "mm").is_none());
    // 族内查得到。
    let note = conversion_source("L", "mL").expect("族内换算应有来源说明");
    assert!(note.contains("liter"));
    assert!(note.contains("1000"));
}

#[test]
fn 换算表只含族内有序对() {
    let table = conversion_table();
    // 长度族 3 单位 + 容量族 3 单位 → 9 + 9 = 18 对。
    assert_eq!(table.len(), 18);
    for c in table {
        let from_family = ultrasound_report_struct::coerce::units::canonical_unit(c.from)
            .expect("表内源单位应在目录内")
            .family;
        let to_family = ultrasound_report_struct::coerce::units::canonical_unit(c.to)
            .expect("表内目标单位应在目录内")
            .family;
        assert_eq!(from_family, to_family, "{} → {} 不该在换算表内", c.from, c.to);
    }
}

#[test]
fn 判定内核与换算层同源() {
    // 同一条输入：判定层与换算层必须给出一致的结论。
    for (value, unit) in [(3.2, "cm"), (1.0, "L"), (0.0, "mL"), (-3.2, "cm"), (3.2, "xx")] {
        let decision = decide(value, None, unit);
        let converted = convert_to_target(value, unit);
        match (decision.value_si, converted) {
            (Some(v), Some((cv, code))) => {
                assert_eq!(v, cv, "{value} {unit} 判定值与换算值应一致");
                assert_eq!(decision.unit_code.as_deref(), Some(code));
            }
            (None, None) => {
                assert_eq!(decision.path, CoercionPath::None);
                assert!(decision.failure.is_some());
            }
            _ => panic!("{value} {unit} 判定层与换算层结论不一致"),
        }
    }
    // 负值在判定层同样判无路径（不产出数值）。
    let neg = decide(-3.2, None, "cm");
    assert_eq!(neg.path, CoercionPath::None);
    assert_eq!(neg.failure, Some(CoercionFailure::ValueInvalid));
    assert!(neg.value_si.is_none());
}
