//! 自由文本语料 × 人工标注表：抽取结果**逐条**对拍。
//!
//! 判据是 `assets/samples/annotations.csv`（人工标注）与 `assets/samples/reports/*.txt`
//! （30 份脱敏语料）。**对照表才是判据** —— 「跑通」不等于「对」：解析内核改动后
//! 若数值、部位、测项、强转路径或原文位置有一处漂移，这条用例就会红。
//!
//! 与 `tests/lexer_tokens.rs` 的分工：那条管**词法层**（token 序列与字符光标），
//! 这条管**抽取层**（结构化条目与失败条目）。
//!
//! 标注表的约定（见 `assets/samples/README.md`）：
//! - `value_raw` / `value_si` 按**数值**记（台账 CSV 的数字格式，3 位小数）；
//! - `ITEM_DICT_MISSING` 挂在条目上（note），不是独立失败行；
//! - 查不到合法强转路径的行，`value_si` / `unit_code` 留空 —— 内核**不产出数值**。

use std::collections::BTreeMap;
use std::path::PathBuf;

use ultrasound_report_struct::parse::parse_report;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// 数值比较（标注表按 3 位小数记，台账 CSV 同口径）。
fn num_eq(a: f64, b: &str) -> bool {
    match b.parse::<f64>() {
        Ok(v) => (a - v).abs() < 0.0005,
        Err(_) => false,
    }
}

#[test]
fn 语料抽取结果与人工标注表逐条一致() {
    let csv = std::fs::read_to_string(manifest_dir().join("assets/samples/annotations.csv"))
        .expect("读人工标注表 assets/samples/annotations.csv");
    let mut entries: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
    let mut failures: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
    let mut expected_rows = 0usize;
    for (i, line) in csv.lines().enumerate() {
        if i == 0 || line.trim().is_empty() {
            continue;
        }
        let f: Vec<String> = line.split(',').map(|s| s.to_string()).collect();
        assert_eq!(f.len(), 14, "标注表第 {} 行列数不对", i + 1);
        expected_rows += 1;
        let kind = f[2].clone();
        let study = f[0].clone();
        if kind == "entry" {
            entries.entry(study).or_default().push(f);
        } else {
            failures.entry(study).or_default().push(f);
        }
    }
    assert!(expected_rows > 0, "人工标注表是空的");

    let mut checked = 0usize;
    let mut problems: Vec<String> = Vec::new();

    for (study, rows) in &entries {
        let rel = rows[0][1].clone();
        let text = std::fs::read_to_string(manifest_dir().join("assets/samples").join(&rel))
            .expect("读语料");
        let out = parse_report(&text);
        let mut items: Vec<_> = out.items.iter().collect();
        items.sort_by_key(|i| (i.line, i.column));
        assert_eq!(
            items.len(),
            rows.len(),
            "{} 条目数与标注不符（实际 {} 条，标注 {} 条）",
            study,
            items.len(),
            rows.len()
        );
        for (item, row) in items.iter().zip(rows.iter()) {
            checked += 1;
            let mut bad = |m: String| problems.push(format!("{} {} {}", study, rel, m));
            if item.part_key.as_deref().unwrap_or("") != row[3] {
                bad(format!("部位键 期望 {} 实际 {:?}", row[3], item.part_key));
            }
            if item.item_code.as_deref().unwrap_or("") != row[4] {
                bad(format!("测项码 期望 {} 实际 {:?}", row[4], item.item_code));
            }
            if !item.value_raw.map(|v| num_eq(v, &row[5])).unwrap_or(false) {
                bad(format!("原值 期望 {} 实际 {:?}", row[5], item.value_raw));
            }
            if item.unit_raw.as_deref().unwrap_or("") != row[6] {
                bad(format!("原单位 期望 {} 实际 {:?}", row[6], item.unit_raw));
            }
            if !item.value_si.map(|v| num_eq(v, &row[7])).unwrap_or(false) {
                // 查不到合法强转路径时标注留空 —— 内核必须**不产出数值**。
                if !(row[7].is_empty() && item.value_si.is_none()) {
                    bad(format!("归一值 期望 {} 实际 {:?}", row[7], item.value_si));
                }
            }
            if item.unit_code.as_deref().unwrap_or("") != row[8] {
                bad(format!("单位码 期望 {} 实际 {:?}", row[8], item.unit_code));
            }
            if item.coercion_path.as_str() != row[9] {
                bad(format!(
                    "强转路径 期望 {} 实际 {}",
                    row[9],
                    item.coercion_path.as_str()
                ));
            }
            if item.line.to_string() != row[10] || item.column.to_string() != row[11] {
                bad(format!(
                    "行列 期望 {}:{} 实际 {}:{}",
                    row[10], row[11], item.line, item.column
                ));
            }
            // 标注列出的 note 必须出现；标注留空则条目不应挂任何 note。
            for code in row[12].split(';').filter(|s| !s.is_empty()) {
                if !item.notes.iter().any(|n| n == code) {
                    bad(format!("缺 note {}（notes={:?}）", code, item.notes));
                }
            }
            if row[12].is_empty() && !item.notes.is_empty() {
                bad(format!("标注未列 note，实际挂 {:?}", item.notes));
            }
        }
    }

    for (study, rows) in &failures {
        let rel = rows[0][1].clone();
        let text = std::fs::read_to_string(manifest_dir().join("assets/samples").join(&rel))
            .expect("读语料");
        let out = parse_report(&text);
        let mut fails: Vec<_> = out.failures.iter().collect();
        fails.sort_by_key(|f| (f.line, f.column));
        assert_eq!(
            fails.len(),
            rows.len(),
            "{} 失败条目数与标注不符（实际 {} 条，标注 {} 条）",
            study,
            fails.len(),
            rows.len()
        );
        for (f, row) in fails.iter().zip(rows.iter()) {
            checked += 1;
            if f.code.as_str() != row[12] {
                problems.push(format!(
                    "{} {} 失败码 期望 {} 实际 {}",
                    study,
                    rel,
                    row[12],
                    f.code.as_str()
                ));
            }
            if f.line.to_string() != row[10] || f.column.to_string() != row[11] {
                problems.push(format!(
                    "{} {} 失败行列 期望 {}:{} 实际 {}:{}",
                    study, rel, row[10], row[11], f.line, f.column
                ));
            }
            if f.raw_snippet != row[13] {
                problems.push(format!(
                    "{} {} 失败片段 期望 {} 实际 {}",
                    study, rel, row[13], f.raw_snippet
                ));
            }
        }
    }

    assert!(
        problems.is_empty(),
        "语料对拍出现 {} 处不符：\n{}",
        problems.len(),
        problems.join("\n")
    );
    assert_eq!(
        checked, expected_rows,
        "对拍行数应与标注表数据行数一致（标注 {} 行，实对拍 {} 行）",
        expected_rows, checked
    );
}
