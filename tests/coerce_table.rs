//! 强转内核第 4 层：**强转路径对照表**的逐条验收。
//!
//! 判据是 `assets/samples/coercion_table.csv`（37 行人工标注的对照表）：
//! 每一行给出「报告原值词面 + 原单位词面 → 期望强转路径 + 期望归一值 + 期望单位码」，
//! 逐行驱动判定内核（`coerce::path::coerce_raw`）**逐条断言**。
//!
//! **对照表才是判据** —— 「跑通」不等于「对」：判定内核、单位目录、名字层
//! 任一处改动后，若归一值或路径有一处漂移，这条用例立刻红。
//!
//! 与 `tests/coerce_units.rs` 的分工：那条守**换算层**（族内/跨族/边界/精度），
//! 这条守**路径判定**（四类路径的逐条口径）。
//!
//! 覆盖要求：四类路径（`UnitConvert` / `DirectScale` / `Composite` / `None`）
//! 各 ≥ 5 行；`None` 行额外断言**不产出数值**（fail-closed）。

use std::path::PathBuf;

use ultrasound_report_struct::coerce::path::CoercionPath;
use ultrasound_report_struct::coerce::table::{CoercionTable, TableError};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// 读仓库内置的强转路径对照表。
fn load_table() -> CoercionTable {
    CoercionTable::load(&manifest_dir().join("assets/samples/coercion_table.csv"))
        .expect("读强转路径对照表 assets/samples/coercion_table.csv")
}

#[test]
fn 对照表行数达标且四类路径各有覆盖() {
    let table = load_table();
    assert!(
        table.len() >= 30,
        "对照表至少 30 行，实际 {} 行",
        table.len()
    );
    let counts = table.count_by_path();
    for path in CoercionPath::ALL {
        let n = counts.get(path.as_str()).copied().unwrap_or(0);
        assert!(
            n >= 5,
            "路径 {} 在对照表里只有 {} 行，至少 5 行",
            path.as_str(),
            n
        );
    }
}

#[test]
fn 对照表逐行全部命中() {
    let table = load_table();
    let mismatches = table.check();
    assert!(
        mismatches.is_empty(),
        "对照表有 {} 行不符：\n{}",
        mismatches.len(),
        ultrasound_report_struct::coerce::table::report(&mismatches, table.len())
    );
    assert!(
        table.assert_all().is_ok(),
        "对照表应整体命中：{:?}",
        table.assert_all()
    );
}

#[test]
fn 失败路径行不产出数值() {
    let table = load_table();
    let none_rows: Vec<_> = table
        .rows()
        .iter()
        .filter(|r| r.expected_path == CoercionPath::None)
        .collect();
    assert!(
        none_rows.len() >= 5,
        "None 行至少 5 行，实际 {} 行",
        none_rows.len()
    );
    for row in none_rows {
        // 期望列留空 —— 表的约定就是「查不到合法路径不产出数值」。
        assert!(
            row.expected_value_si.is_none(),
            "第 {} 行期望归一值应留空",
            row.line_no
        );
        assert!(
            row.expected_unit_code.is_none(),
            "第 {} 行期望单位码应留空",
            row.line_no
        );
        // 内核同样不产出数值，且给出失败原因。
        let decision = row.drive();
        assert_eq!(decision.path, CoercionPath::None, "第 {} 行", row.line_no);
        assert!(
            decision.value_si.is_none(),
            "第 {} 行不应产出数值",
            row.line_no
        );
        assert!(
            decision.unit_code.is_none(),
            "第 {} 行不应产出单位码",
            row.line_no
        );
        assert!(
            decision.failure.is_some(),
            "第 {} 行应给出失败原因（不静默兜底）",
            row.line_no
        );
        assert!(decision.steps.is_empty(), "第 {} 行不应有步骤链", row.line_no);
    }
}

#[test]
fn 成功路径行产出数值且步骤链自洽() {
    let table = load_table();
    for row in table.rows() {
        if row.expected_path == CoercionPath::None {
            continue;
        }
        let decision = row.drive();
        assert!(
            decision.is_ok(),
            "第 {} 行应判出合法路径，实际 {:?}",
            row.line_no,
            decision
        );
        assert!(
            decision.is_consistent(),
            "第 {} 行路径与步骤链不自洽：{}",
            row.line_no,
            decision.describe()
        );
        let expected = row
            .expected_value_si
            .expect("非 None 行应有期望归一值");
        let actual = decision.value_si.expect("非 None 行应产出归一值");
        assert!(
            (actual - expected).abs() < 1e-9,
            "第 {} 行归一值不符：期望 {}，实际 {}",
            row.line_no,
            expected,
            actual
        );
    }
}

#[test]
fn 对拍不一致时报告含行号与期望实际() {
    // 故意写错一行：期望归一值改成 99，判定内核给的是 28。
    let csv = "value_raw,unit_raw,expected_path,expected_value_si,expected_unit_code,note\n\
3.2,cm,UnitConvert,32,mm,长度族：厘米→毫米\n\
2.8,cm,UnitConvert,99,mm,故意写错的期望值\n";
    let table = CoercionTable::parse(csv).expect("应能解析");
    let mismatches = table.check();
    assert_eq!(mismatches.len(), 1, "只有写错的那一行不符");
    let m = &mismatches[0];
    assert_eq!(m.line_no, 3);
    assert_eq!(m.input, "2.8 cm");
    assert!(m.reasons.contains(&"归一值"));
    assert!(m.expected.contains("99"));
    assert!(m.actual.contains("28"));

    let text = ultrasound_report_struct::coerce::table::report(&mismatches, table.len());
    assert!(text.contains("第 3 行"), "报告须含行号：{}", text);
    assert!(text.contains("2.8 cm"), "报告须含输入：{}", text);
    assert!(text.contains("期望"), "报告须含期望列：{}", text);
    assert!(text.contains("实际"), "报告须含实际列：{}", text);

    let err = table.assert_all().expect_err("不匹配应返回 Err");
    assert!(err.contains("第 3 行"));
}

#[test]
fn 表头列序错位会被拒() {
    // 列序错位会让人按列号读错值 —— 比没有对照表更糟，必须显式拒绝。
    let bad = "unit_raw,value_raw,expected_path,expected_value_si,expected_unit_code,note\n\
cm,3.2,UnitConvert,32,mm,\n";
    let err = CoercionTable::parse(bad).expect_err("列序错位应报错");
    assert!(matches!(err, TableError::Header(_)));
}

#[test]
fn 仓库对照表可从文件读取且与内联解析一致() {
    let path = manifest_dir().join("assets/samples/coercion_table.csv");
    let from_file = CoercionTable::load(&path).expect("从文件读对照表");
    let from_text = CoercionTable::parse(&std::fs::read_to_string(&path).expect("读 CSV"))
        .expect("从文本解析对照表");
    assert_eq!(from_file.len(), from_text.len());
    assert_eq!(from_file.rows(), from_text.rows());
}
