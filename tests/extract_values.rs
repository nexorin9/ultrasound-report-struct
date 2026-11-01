//! 解析内核第 5 层：数值与单位抽取 —— 端到端用例。
//!
//! 判据是**抽取结果本身**：原值（`value_raw`）、原单位（`unit_raw`）与原文位置
//! （1 基行列）。这三样是台账行的来源，也是强转内核判定路径的前提 ——
//! 单位词面丢了，「3.2 是 cm 还是 mm」就无从判定。
//!
//! 与 `tests/corpus_annotations.rs` 的分工：那条管**整份脱敏语料**与人工标注表
//! 逐条对拍（30 份报告 × 62 行），这条管**数值与单位这一层**的边界写法
//! （带空格、全角、缺单位、坏值、前置小数点）。

use ultrasound_report_struct::parse::{
    classify_values, parse_measurement_value, ErrorCode, ValueDefect, ValueParse, ValueVerdict,
};
use ultrasound_report_struct::parse::{lexer, lookahead};
use ultrasound_report_struct::parse::parse_report;

/// 合并后的 token 序列（用例入口）。
fn merged(text: &str) -> Vec<lookahead::Merged> {
    lookahead::merge(&lexer::tokenize(text))
}

/// 数值层判定（用例入口）。
fn verdicts(text: &str) -> Vec<ValueVerdict> {
    classify_values(&merged(text), text)
}

/// 第一条测量候选（用例入口）。
fn measured(text: &str) -> ultrasound_report_struct::parse::ValueHit {
    verdicts(text)
        .into_iter()
        .find_map(|v| match v {
            ValueVerdict::Measured(h) => Some(h),
            _ => None,
        })
        .expect("应产出测量候选")
}

/// 第一条缺单位候选（用例入口）。
fn missing(text: &str) -> ultrasound_report_struct::parse::ValueHit {
    verdicts(text)
        .into_iter()
        .find_map(|v| match v {
            ValueVerdict::MissingUnit(h) => Some(h),
            _ => None,
        })
        .expect("应产出缺单位候选")
}

#[test]
fn 四种常见写法都抽得出原值原单位() {
    // 「3.2cm」：数值与单位紧邻。
    let a = measured("左肾积水 3.2cm");
    assert_eq!(a.value, Some(3.2));
    assert_eq!(a.value_text, "3.2");
    assert_eq!(a.unit.as_deref(), Some("cm"));

    // 「3.2 cm」：中间留空格 —— 空格不是测量的一部分。
    let b = measured("左肾积水 3.2 cm");
    assert_eq!(b.value, Some(3.2));
    assert_eq!(b.value_text, "3.2");
    assert_eq!(b.unit.as_deref(), Some("cm"));

    // 「32mm」：已经是院内口径的单位，原样保留。
    let c = measured("左肾长 32mm");
    assert_eq!(c.value, Some(32.0));
    assert_eq!(c.unit.as_deref(), Some("mm"));

    // 「残余尿量 约 120ml」：修饰词「约」不参与数值抽取。
    let d = measured("膀胱残余尿量 约 120ml");
    assert_eq!(d.value, Some(120.0));
    assert_eq!(d.value_text, "120");
    assert_eq!(d.unit.as_deref(), Some("ml"));
}

#[test]
fn 原文位置逐条对得上() {
    let hit = measured("超声所见\n左肾积水 3.2cm");
    assert_eq!(hit.line, 2, "第 2 行");
    assert_eq!(hit.column, 6, "「左肾积水 」5 个字符，数值从第 6 列起");
    // 数值与单位**各留各的位置**：「超声所见」4 字 + 换行 1 字 + 「左肾积水 」5 字
    // = 数值从第 11 个字符起，单位从第 14 个字符起。
    assert_eq!(hit.start, 11);
    assert_eq!(hit.unit_start, Some(14));
}

#[test]
fn 缺单位产出失败条目且原值保留() {
    let out = parse_report("左肾积水 3.2");
    assert!(out.items.is_empty(), "缺单位不产出数值");
    assert_eq!(out.failures.len(), 1);
    let failure = &out.failures[0];
    assert_eq!(failure.code, ErrorCode::UnitMissing);
    assert_eq!(failure.raw_snippet, "3.2", "片段是原值词面");
    assert_eq!(failure.line, 1);
    assert_eq!(failure.column, 6);

    // 原值保留在数值候选里 —— 运维要看到「哪个数没单位」。
    let hit = missing("左肾积水 3.2");
    assert_eq!(hit.value, Some(3.2));
    assert_eq!(hit.unit, None);
}

#[test]
fn 四类坏值各有明确错误码与细分归类() {
    // 多个小数点。
    let out = parse_report("左肾积水 3.2.5cm");
    assert!(out.items.is_empty());
    assert_eq!(out.failures.len(), 1);
    assert_eq!(out.failures[0].code, ErrorCode::ValueUnparseable);
    assert_eq!(out.failures[0].detail, ValueDefect::MultipleDots.as_str());

    // 超长数字串（长度超过院内常识上限 15 位）。
    let long = "1".repeat(16);
    let out = parse_report(&format!("左肾积水 {}cm", long));
    assert!(out.items.is_empty(), "超长数字串不产出数值");
    assert_eq!(out.failures[0].code, ErrorCode::ValueUnparseable);
    assert_eq!(out.failures[0].detail, ValueDefect::TooLong.as_str());

    // 空值与纯符号：词法层不会产出这类 token（防御性归类，仍各有明确归类）。
    assert_eq!(
        parse_measurement_value("").defect(),
        Some(ValueDefect::Empty)
    );
    assert_eq!(
        parse_measurement_value(".").defect(),
        Some(ValueDefect::PureSign)
    );
    assert_eq!(
        parse_measurement_value("-").defect(),
        Some(ValueDefect::PureSign)
    );
}

#[test]
fn 坏值不静默改数据正常值照常产出() {
    // 一份报告里有一条好值、一条坏值：坏值进失败清单，好值照常进台账。
    let out = parse_report("左肾积水 3.2cm\n右肾积水 3.2.5cm");
    assert_eq!(out.items.len(), 1, "好值照常产出");
    assert_eq!(out.items[0].value_raw, Some(3.2));
    assert_eq!(out.items[0].unit_raw.as_deref(), Some("cm"));
    assert_eq!(out.items[0].value_si, Some(32.0));
    assert_eq!(out.failures.len(), 1, "坏值只坏自己那一条");
    assert_eq!(out.failures[0].code, ErrorCode::ValueUnparseable);
}

#[test]
fn 前置小数点不被读成差十倍() {
    // 「.5cm」按 5cm 收下的话，台账里的数字看着完全正常，只是比错了对象。
    let out = parse_report("左肾积水 .5cm");
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0].value_raw, Some(0.5));
    assert_eq!(out.items[0].value_si, Some(5.0));
    assert_eq!(out.items[0].unit_raw.as_deref(), Some("cm"));
}

#[test]
fn 原值原单位一路透传到台账行() {
    // 台账行的 value_raw / unit_raw 就是这一层交出去的东西：
    // 强转内核按**原单位词面**查目录，原样保留才判得出路径。
    let out = parse_report("左肾积水 3.2CM");
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0].value_raw, Some(3.2));
    assert_eq!(
        out.items[0].unit_raw.as_deref(),
        Some("CM"),
        "原单位词面照原文保留（大小写归一是单位目录那一层的职责）"
    );
    assert_eq!(out.items[0].value_si, Some(32.0), "大小写不敏感，院内口径仍是 mm");
    assert_eq!(out.items[0].unit_code.as_deref(), Some("mm"));
}

#[test]
fn 数值分类器逐条断言() {
    assert_eq!(parse_measurement_value("3.2"), ValueParse::Ok(3.2));
    assert_eq!(parse_measurement_value("32"), ValueParse::Ok(32.0));
    assert_eq!(parse_measurement_value(".5"), ValueParse::Ok(0.5));
    // 全角数字归一后与半角同义。
    assert_eq!(parse_measurement_value("３.２"), ValueParse::Ok(3.2));
    // 非数值词面明确拒绝。
    assert_eq!(
        parse_measurement_value("3.2cm").defect(),
        Some(ValueDefect::NotNumeric)
    );
}
