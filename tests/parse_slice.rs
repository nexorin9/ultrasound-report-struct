//! 主干切片 A 的集成验收：词法 → 前瞻合并 → 抽取 → 最小强转。
//!
//! 对应源产品主路径「外部杂乱文本 → 结构化对象」的最小纵切：
//! 一条输入一个解析入口，产出带原文位置的结构化条目。

use ultrasound_report_struct::coerce::CoercionPath;
use ultrasound_report_struct::parse::{parse_report, ErrorCode};

#[test]
fn 左肾三二厘米抽出一条并走单位换算() {
    let outcome = parse_report("左肾 3.2cm");
    assert_eq!(outcome.items.len(), 1, "应抽出一条结构化条目");
    let item = &outcome.items[0];
    assert_eq!(item.part_key.as_deref(), Some("kidney.left"));
    assert_eq!(item.value_raw, Some(3.2));
    assert_eq!(item.unit_raw.as_deref(), Some("cm"));
    assert_eq!(item.value_si, Some(32.0));
    assert_eq!(item.unit_code.as_deref(), Some("mm"));
    assert_eq!(item.coercion_path, CoercionPath::UnitConvert);
    assert!(outcome.failures.is_empty(), "正常报告不应有失败条目");
}

#[test]
fn 缺单位产出失败条目且不产出数值() {
    let outcome = parse_report("左肾 3.2");
    assert!(outcome.items.is_empty(), "缺单位不得产出数值条目");
    assert_eq!(outcome.failures.len(), 1);
    let failure = &outcome.failures[0];
    assert_eq!(failure.code, ErrorCode::UnitMissing);
    assert_eq!(failure.raw_snippet, "3.2");
    assert!(failure.line >= 1, "失败条目须带行号");
    assert!(failure.column >= 1, "失败条目须带列号");
}

#[test]
fn 左肾积水三二厘米抽出单条并保留原单位() {
    let outcome = parse_report("左肾积水 3.2cm");
    assert_eq!(outcome.items.len(), 1);
    let item = &outcome.items[0];
    assert_eq!(item.part_key.as_deref(), Some("kidney.left"));
    assert_eq!(item.value_raw, Some(3.2));
    assert_eq!(item.unit_raw.as_deref(), Some("cm"));
    assert_eq!(item.coercion_path, CoercionPath::UnitConvert);
}

#[test]
fn 失败条目指回原文行列() {
    let text = "超声所见\n左肾积水 3.2\n膀胱充盈可";
    let outcome = parse_report(text);
    assert_eq!(outcome.failures.len(), 1);
    let failure = &outcome.failures[0];
    // 失败在第 2 行第 6 列（字符计）
    assert_eq!(failure.line, 2);
    assert_eq!(failure.column, 6);
}

/// 部位归一（Task 14）：写法不同，键必须相同 —— 否则对拍会拿左肾比右肾。
#[test]
fn 部位别名写法归一到同一规范键() {
    for text in ["左肾 3.2cm", "左侧肾脏 3.2cm", "左肾脏 3.2cm"] {
        let outcome = parse_report(text);
        assert_eq!(
            outcome.items[0].part_key.as_deref(),
            Some("kidney.left"),
            "{} 应归一到 kidney.left",
            text
        );
    }
    for text in ["右肾 3.2cm", "右侧肾脏 3.2cm"] {
        let outcome = parse_report(text);
        assert_eq!(outcome.items[0].part_key.as_deref(), Some("kidney.right"));
    }
    // 双肾 ≡ 双侧肾 ≡ 同一双侧键。
    for text in ["双肾积水 2.6cm", "双侧肾积水 2.6cm"] {
        let outcome = parse_report(text);
        assert_eq!(
            outcome.items[0].part_key.as_deref(),
            Some("kidney.bilateral"),
            "{} 应归一到 kidney.bilateral",
            text
        );
    }
    // 输尿管左右、膀胱无侧别。
    assert_eq!(
        parse_report("左侧输尿管内径 0.6cm").items[0]
            .part_key
            .as_deref(),
        Some("ureter.left")
    );
    assert_eq!(
        parse_report("右侧输尿管内径 0.7cm").items[0]
            .part_key
            .as_deref(),
        Some("ureter.right")
    );
    assert_eq!(
        parse_report("膀胱 2.5cm").items[0].part_key.as_deref(),
        Some("bladder")
    );
}

/// 词典外的部位词：未命中、显式失败、不产出部位键 —— 宁可不比不错比。
#[test]
fn 词典外部位词产出未命中失败条目且不带部位键() {
    let outcome = parse_report("超声所见\n胆囊 3.2cm");
    assert_eq!(outcome.items.len(), 1);
    assert_eq!(outcome.items[0].part_key, None);
    assert_eq!(outcome.items[0].item_code, None);
    assert_eq!(outcome.failures.len(), 1);
    let failure = &outcome.failures[0];
    assert_eq!(failure.code, ErrorCode::PartUnresolved);
    // 失败指回**部位词自身**（第 2 行第 1 列），运维一眼知道要补哪个词。
    assert_eq!(failure.line, 2);
    assert_eq!(failure.column, 1);
    assert_eq!(failure.raw_snippet, "胆囊");
}

/// 肾盂与肾是两个解剖基名：测项码不串用，历史值不会互相比较。
#[test]
fn 肾盂与肾的合成键互不串用() {
    let a = parse_report("左肾盂前后径 2.1cm");
    assert_eq!(a.items[0].part_key.as_deref(), Some("renal_pelvis.left"));
    assert_eq!(
        a.items[0].item_code.as_deref(),
        Some("renal_pelvis.ap_diameter")
    );
    let b = parse_report("左肾前后径 2.1cm");
    assert_eq!(b.items[0].part_key.as_deref(), Some("kidney.left"));
    assert_eq!(b.items[0].item_code.as_deref(), Some("kidney.ap_diameter"));
}

/// 测项归一（Task 15）：写法不同，测项码必须相同 —— 否则同侧历史值比不到一起。
#[test]
fn 测项多种写法归一到同一测项码() {
    for (text, expect) in [
        ("左肾长 8.5cm", "kidney.length"),
        ("左肾长度 8.5cm", "kidney.length"),
        ("左肾宽 4.3cm", "kidney.width"),
        ("左肾宽度 4.3cm", "kidney.width"),
        ("左肾前后径 2.1cm", "kidney.ap_diameter"),
        ("左肾盂前后径 2.1cm", "renal_pelvis.ap_diameter"),
        ("膀胱残余尿量 120ml", "bladder.residual_urine_volume"),
        ("左侧输尿管内径 0.6cm", "ureter.inner_diameter"),
    ] {
        let outcome = parse_report(text);
        assert_eq!(outcome.items.len(), 1, "{} 应抽出一条", text);
        assert_eq!(
            outcome.items[0].item_code.as_deref(),
            Some(expect),
            "{} 的测项码应归一到 {}",
            text,
            expect
        );
        assert!(
            outcome.failures.is_empty(),
            "{} 不应有失败条目：{:?}",
            text,
            outcome.failures
        );
    }
}

/// 测项词典未命中：产出**带行列的失败条目**并指回测项词 —— 运维照着补词典。
#[test]
fn 测项词典未命中产出失败条目并指回测项词() {
    let text = "超声所见\n膀胱前后径 2.1cm";
    let outcome = parse_report(text);
    assert_eq!(outcome.items.len(), 1, "测量本身照常产出条目");
    assert_eq!(outcome.items[0].item_code, None, "未命中不得产出测项码");
    assert!(
        outcome.items[0]
            .notes
            .iter()
            .any(|n| n == ErrorCode::ItemDictMissing.as_str()),
        "条目应挂 ITEM_DICT_MISSING 备注"
    );
    assert_eq!(outcome.failures.len(), 1, "未命中要进失败清单");
    let failure = &outcome.failures[0];
    assert_eq!(failure.code, ErrorCode::ItemDictMissing);
    // 失败指回**测项词自身**（第 2 行第 3 列），不是指到数值。
    assert_eq!(failure.line, 2);
    assert_eq!(failure.column, 3);
    assert_eq!(failure.raw_snippet, "前后径");
}

/// 重复测项：同一部位同一测项出现两次是**两条记录**，去重会让后一次凭空消失。
#[test]
fn 同一部位同一测项出现两次两条都保留() {
    let outcome = parse_report("左肾前后径 2.1cm\n左肾前后径 2.3cm");
    assert_eq!(outcome.items.len(), 2, "重复测项不得被去重");
    for item in &outcome.items {
        assert_eq!(item.item_code.as_deref(), Some("kidney.ap_diameter"));
        assert_eq!(item.part_key.as_deref(), Some("kidney.left"));
    }
    assert_ne!(outcome.items[0].line, outcome.items[1].line, "两条各带各的位置");
    assert_eq!(outcome.items[0].line, 1);
    assert_eq!(outcome.items[1].line, 2);
}
