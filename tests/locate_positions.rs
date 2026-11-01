//! 失败字符光标定位：位置矩阵 + 四类失败分支接线 + 失败清单可反查复核。
//!
//! **这一层要证明什么**：失败清单里的「第几行第几列」是运维唯一的抓手 ——
//! 它得能照着原文一眼找到那一处。因此本条用例管三件事：
//!
//! 1. **位置矩阵与人工标注一致**（`assets/samples/locate_positions.csv` +
//!    `assets/samples/locate_cases/*.txt`）—— 判据是**人工标注**，
//!    不是实现自己的输出；中文多字节 / 全角括号 / 空行 / 超长行四类样例各有期望行列。
//! 2. **四类失败分支都接上了位置** —— 单位缺失 / 部位未命中 / 测项未命中 /
//!    数值不可解析，各产出一条带 1 基行列的失败条目。
//! 3. **失败清单可独立复核** —— 每条失败条目的「行 + 列 + 原文片段」都能
//!    反查回原文的同一处（`char_index_at`），不必相信解析器。
//!
//! 与 `tests/corpus_annotations.rs` 的分工：那条管**抽取语义**（部位 / 测项 / 数值 /
//! 强转路径），这条管**位置语义**（行列本身对不对、能不能复核回原文）。

use std::path::PathBuf;

use ultrasound_report_struct::parse::locate::{char_index_at, locate, snippet_at};
use ultrasound_report_struct::parse::parse_report;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// 读样例正文（严格按字节读，不做换行转换 —— CRLF 样例要看到真实的 `\r\n`）。
fn read_sample(rel: &str) -> String {
    let bytes = std::fs::read(manifest_dir().join("assets/samples").join(rel))
        .unwrap_or_else(|e| panic!("读样例 {} 失败：{}", rel, e));
    String::from_utf8(bytes).unwrap_or_else(|e| panic!("样例 {} 不是合法 UTF-8：{}", rel, e))
}

#[test]
fn 位置矩阵与人工标注一致() {
    let csv = std::fs::read_to_string(manifest_dir().join("assets/samples/locate_positions.csv"))
        .expect("读人工标注表 assets/samples/locate_positions.csv");
    let mut checked = 0usize;
    let mut problems: Vec<String> = Vec::new();

    for (i, line) in csv.lines().enumerate() {
        if i == 0 || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(',').collect();
        assert_eq!(f.len(), 6, "标注表第 {} 行列数不对", i + 1);
        let (case_id, text_file, char_index, expect_line, expect_column) =
            (f[0], f[1], f[2], f[3], f[4]);
        let char_index: usize = char_index
            .parse()
            .unwrap_or_else(|_| panic!("{} 的 char_index 不是整数：{}", case_id, char_index));
        let expect_line: usize = expect_line
            .parse()
            .unwrap_or_else(|_| panic!("{} 的 expect_line 不是整数：{}", case_id, expect_line));
        let expect_column: usize = expect_column
            .parse()
            .unwrap_or_else(|_| panic!("{} 的 expect_column 不是整数：{}", case_id, expect_column));

        let text = read_sample(text_file);
        let (line, column) = locate(&text, char_index);
        checked += 1;
        if line != expect_line || column != expect_column {
            problems.push(format!(
                "{} {} 字符 #{} 期望 {}:{} 实际 {}:{}",
                case_id, text_file, char_index, expect_line, expect_column, line, column
            ));
        }
    }

    assert!(checked >= 40, "位置矩阵样本过少：{} 行", checked);
    assert!(
        problems.is_empty(),
        "位置矩阵出现 {} 处不符：\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn 四类失败分支都产出带位置的失败条目() {
    // 单位缺失：数值有、单位没有 —— 指回**数值**。
    let out = parse_report("超声所见\n左肾积水 3.2");
    assert_eq!(out.failures.len(), 1, "应恰有一条失败");
    let f = &out.failures[0];
    assert_eq!(f.code.as_str(), "UNIT_MISSING");
    assert_eq!((f.line, f.column), (2, 6), "数值 3.2 在第 2 行第 6 列");
    assert_eq!(f.raw_snippet, "3.2");

    // 部位未命中：词典查不到部位 —— 指回**部位词**。
    let out = parse_report("超声所见\n胆囊 3.2cm");
    assert_eq!(out.failures.len(), 1);
    let f = &out.failures[0];
    assert_eq!(f.code.as_str(), "PART_UNRESOLVED");
    assert_eq!((f.line, f.column), (2, 1), "部位词 胆囊 在第 2 行第 1 列");
    assert_eq!(f.raw_snippet, "胆囊");

    // 测项未命中：测项词写了但词典查不到（前后径 不适用于膀胱）—— 指回**测项词**。
    let out = parse_report("超声所见\n膀胱前后径 2.1cm");
    assert_eq!(out.failures.len(), 1);
    let f = &out.failures[0];
    assert_eq!(f.code.as_str(), "ITEM_DICT_MISSING");
    assert_eq!((f.line, f.column), (2, 3), "测项词 前后径 在第 2 行第 3 列");
    assert_eq!(f.raw_snippet, "前后径");

    // 数值不可解析：多个小数点 —— 指回**数值**。
    let out = parse_report("超声所见\n左肾积水 3.2.5cm");
    assert_eq!(out.failures.len(), 1);
    let f = &out.failures[0];
    assert_eq!(f.code.as_str(), "VALUE_UNPARSEABLE");
    assert_eq!((f.line, f.column), (2, 6), "坏值 3.2.5cm 在第 2 行第 6 列");
    assert_eq!(f.raw_snippet, "3.2.5cm");
}

#[test]
fn 失败清单的每一条都能反查回原文() {
    // 失败清单导出的是「行 + 列 + 原文片段」。运维拿到原始报告后要能独立复核：
    // 反查回字符光标，那一处的原文必须就是失败条目给的片段。
    // 这条断言把「位置」与「片段」绑在一起 —— 位置指错地方，台账就成了假指认。
    let mut checked = 0usize;
    let mut problems: Vec<String> = Vec::new();

    for i in 1..=30 {
        let rel = format!("reports/sample-uro-{:02}.txt", i);
        let text = read_sample(&rel);
        let out = parse_report(&text);
        for f in &out.failures {
            checked += 1;
            if f.line == 0 || f.column == 0 {
                problems.push(format!("{} {} 失败条目没有位置", rel, f.code.as_str()));
                continue;
            }
            let snippet_len = f.raw_snippet.chars().count();
            match snippet_at(&text, f.line, f.column, snippet_len) {
                Some(actual) if actual == f.raw_snippet => {}
                Some(actual) => problems.push(format!(
                    "{} {} {}:{} 处原文 {:?} 与片段 {:?} 不符",
                    rel,
                    f.code.as_str(),
                    f.line,
                    f.column,
                    actual,
                    f.raw_snippet
                )),
                None => problems.push(format!(
                    "{} {} {}:{} 位置反查不到字符",
                    rel,
                    f.code.as_str(),
                    f.line,
                    f.column
                )),
            }
        }
    }

    assert!(
        checked >= 9,
        "语料里的失败条目不该少于 9 条（反查断言覆盖不到就是形同虚设）：实际 {} 条",
        checked
    );
    assert!(
        problems.is_empty(),
        "失败清单反查出 {} 处问题：\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn 条目位置落在数值词面上() {
    // 条目的行列指的是**原值词面**的位置 —— 台账行导出的行列要能带人
    // 回到原文里那个数字。位置落在别处（部位词、单位词、叙述句），
    // 运维按行列去核对就会对不上号。
    let mut checked = 0usize;
    let mut problems: Vec<String> = Vec::new();

    for i in 1..=30 {
        let rel = format!("reports/sample-uro-{:02}.txt", i);
        let text = read_sample(&rel);
        let out = parse_report(&text);
        for item in &out.items {
            checked += 1;
            if item.line == 0 || item.column == 0 {
                problems.push(format!("{} 条目没有位置", rel));
                continue;
            }
            let at = snippet_at(&text, item.line, item.column, 1).unwrap_or_default();
            let ch = at.chars().next().unwrap_or(' ');
            let is_value_head = ch.is_ascii_digit()
                || ch == '.'
                || ch == '-'
                || ch == '+'
                || ch == '−'
                || ('０'..='９').contains(&ch)
                || ch == '．';
            if !is_value_head {
                problems.push(format!(
                    "{} {}:{} 位置落在 {:?} 而不是数值词面",
                    rel, item.line, item.column, ch
                ));
            }
        }
    }

    assert!(checked >= 50, "语料条目过少：{} 条", checked);
    assert!(
        problems.is_empty(),
        "条目位置查出 {} 处问题：\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn 越界与未知位置返回零不编造() {
    let text = "超声所见\n左肾积水 3.2cm";
    let len = text.chars().count();
    assert_eq!(locate(text, 0), (0, 0), "光标 0 是未知 不是第一行第一列");
    assert_eq!(locate(text, len + 1), (0, 0), "越过文本末尾 位置不可确定");
    assert_eq!(locate(text, len), (2, 10), "末字符仍有位置");
    assert_eq!(char_index_at(text, 0, 0), None, "未知位置不可反查");
    assert_eq!(char_index_at(text, 99, 1), None, "不存在的行不可反查");
    assert_eq!(char_index_at(text, 2, 99), None, "越界的列不可反查");
}
