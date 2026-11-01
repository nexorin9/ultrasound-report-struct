//! 对拍引擎第 5 层：**差值基准回归集**的逐条验收。
//!
//! 判据是 `assets/samples/compare_baseline.csv`（人工标注的差值基准表）：
//! 每一行给出「报告当前测值（原值原单位）+ RIS 历史值 → 期望状态 + 期望差值 +
//! 期望方向 + 期望命中条数 + 期望失败码」，逐行驱动**真实对拍主路径**
//! （条目 → 强转 → 规范键匹配 → 同侧规则 → 三态判定 / 降级），**逐条断言**。
//!
//! **这张表才是判据** —— 「跑通」不等于「对」：匹配键改了、同侧规则改了、
//! 强转改了、快照语义改了，若差值或状态有一处漂移，这条用例立刻红。
//!
//! 与前面几条的分工：
//! - `tests/compare_match.rs` 守**单组手算基准**（3 组差值与方向）；
//! - `tests/compare_states.rs` 守**三态判定**（有历史 / 无历史 / 歧义）；
//! - `tests/compare_side_degrade.rs` 守**同侧规则 + 不可达降级**；
//! - 这条守**整张基准表**（32 行，四类状态各有覆盖）—— 把上面几条的结论
//!   固定成一张可逐条复核、可交给医师对账的表。
//!
//! **oracle = RIS 真实历史值**：差值不是自己跟自己比，
//! 而是「报告这次的测值」对「RIS 里该患者同侧部位上次的历史值」。

use std::path::PathBuf;

use ultrasound_report_struct::compare::{
    baseline_report, BaselineTable, ComparisonState, BASELINE_MIN_ROWS,
};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// 读仓库内置的差值基准表。
fn load_table() -> BaselineTable {
    BaselineTable::load(&manifest_dir().join("assets/samples/compare_baseline.csv"))
        .expect("读差值基准表 assets/samples/compare_baseline.csv")
}

#[test]
fn 基准表行数达标且四类状态各有覆盖() {
    let table = load_table();
    assert!(
        table.len() >= BASELINE_MIN_ROWS,
        "基准表至少 {} 行，实际 {} 行",
        BASELINE_MIN_ROWS,
        table.len()
    );
    let counts = table.count_by_state();
    for state in ComparisonState::ALL {
        let n = counts.get(state.as_str()).copied().unwrap_or(0);
        assert!(
            n >= 2,
            "状态 {} 在基准表里只有 {} 行，至少 2 行",
            state.as_str(),
            n
        );
    }
}

#[test]
fn 基准表逐行全部命中() {
    let table = load_table();
    let mismatches = table.check();
    assert!(
        mismatches.is_empty(),
        "差值基准表应逐行命中，实际不符：{}",
        baseline_report(&mismatches, table.len())
    );
    assert!(table.assert_all().is_ok());
}

#[test]
fn 每行都判得出结论且判定口径自洽() {
    // 「判不出结论」只发生在当前测值本身不可用时 ——
    // 基准表每一行都必须给出明确的状态，不许含糊。
    let table = load_table();
    for row in table.rows() {
        let outcome = row.drive();
        assert_eq!(
            outcome.state,
            Some(row.expect_state),
            "第 {} 行（{}）状态不符：期望 {}，实际 {:?}",
            row.line_no,
            row.case_id,
            row.expect_state.as_str(),
            outcome.state.map(|s| s.as_str())
        );
        // 判定口径自洽：只有「有历史」才带差值，且恰好命中一条。
        if row.expect_state == ComparisonState::HasHistory {
            assert_eq!(outcome.hit_count, 1, "第 {} 行命中条数应为 1", row.line_no);
            assert!(outcome.delta.is_some(), "第 {} 行应产出差值", row.line_no);
        } else {
            assert!(
                outcome.delta.is_none(),
                "第 {} 行状态 {} 不该产出差值",
                row.line_no,
                row.expect_state.as_str()
            );
        }
    }
}

#[test]
fn 首次不是失败_跳过不是无差异() {
    // 两条最容易被混起来的口径，在这里钉死：
    // 首次检查是**正常业务结果**（不带失败码）；
    // RIS 够不着是**系统没做成**（带失败码，且不产差值）。
    let table = load_table();
    for row in table.rows() {
        let outcome = row.drive();
        match row.expect_state {
            ComparisonState::FirstTime => {
                assert_eq!(
                    outcome.error_code, None,
                    "第 {} 行：首次检查不该带失败码",
                    row.line_no
                );
            }
            ComparisonState::Skipped => {
                assert!(
                    outcome.error_code.is_some(),
                    "第 {} 行：跳过必须带失败码（不等于无差异）",
                    row.line_no
                );
                assert!(
                    outcome.delta.is_none(),
                    "第 {} 行：跳过不该产出差值",
                    row.line_no
                );
            }
            ComparisonState::Ambiguous => {
                assert_eq!(
                    outcome.error_code,
                    Some(ultrasound_report_struct::parse::ErrorCode::ComparisonAmbiguous),
                    "第 {} 行：歧义必须带 COMPARISON_AMBIGUOUS",
                    row.line_no
                );
                assert!(
                    outcome.delta.is_none(),
                    "第 {} 行：歧义不该产出差值（绝不任取其一）",
                    row.line_no
                );
            }
            ComparisonState::HasHistory => {
                assert_eq!(
                    outcome.error_code, None,
                    "第 {} 行：有历史不是失败",
                    row.line_no
                );
            }
        }
    }
}

#[test]
fn 基准表里没有拟真标识() {
    // 基准表里的检查号 / 患者主索引一律用占位，不出现拟真的住院号或姓名。
    let table = load_table();
    for row in table.rows() {
        assert!(
            row.patient_id.starts_with("PID-"),
            "第 {} 行患者主索引应为占位：{}",
            row.line_no,
            row.patient_id
        );
        for h in &row.history {
            assert!(
                h.study_no.starts_with("S-HIST-"),
                "第 {} 行检查号应为占位：{}",
                row.line_no,
                h.study_no
            );
        }
    }
}
