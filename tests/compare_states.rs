//! 对拍引擎第 3 层：三态判定（有历史 / 无历史 / 多历史歧义）—— 集成用例。
//!
//! **这一组用例守什么**：
//!
//! 1. **整条产品路径**：报告正文 → 解析（部位 / 测项 / 数值归一）→ 三态判定，
//!    历史值用**RIS 会返回的那种 JSON**（不是手工构造的结构体）。
//! 2. **三态各有断言**：有历史出差值；无历史判「首次」且**不算失败**；
//!    多历史歧义**不出差值**、出失败条目。
//! 3. **歧义的危险性用数据说清**：两条候选差值不同（28mm 与 30mm），
//!    任取其一都会给出一个**看起来正常却可能指错对象**的差值 ——
//!    这正是本层不任取其一的原因。
//! 4. **失败条目指得回原文**：歧义条目带行列与原样词面（`3.2cm`），
//!    医师照着就能在报告里找到那一条。

use ultrasound_report_struct::compare::{
    judge_item, ComparisonState, Direction, RisReport,
};
use ultrasound_report_struct::parse::{parse_report, DetailCode, ErrorCode};

/// 一条能解析出规范键与归一值的报告正文（泌尿系，脱敏）。
const REPORT_TEXT: &str = "超声所见\n左肾前后径 3.2cm\n";

/// RIS 会返回的那种历史报文：同侧同测项的**一份**历史。
const HISTORY_ONE: &str = r#"[
  {
    "study_no": "S-HIST-201",
    "patient_id": "PID-EXAMPLE",
    "body_part": "kidney.left",
    "performed_at": "2026-01-08",
    "measurements": [
      {"item_code": "kidney.ap_diameter", "value": 28, "unit": "mm"}
    ]
  }
]"#;

/// RIS 会返回的那种历史报文：同侧同测项的**两份**历史（歧义）。
const HISTORY_TWO: &str = r#"[
  {
    "study_no": "S-HIST-202",
    "patient_id": "PID-EXAMPLE",
    "body_part": "kidney.left",
    "performed_at": "2026-01-08",
    "measurements": [
      {"item_code": "kidney.ap_diameter", "value": 28, "unit": "mm"}
    ]
  },
  {
    "study_no": "S-HIST-203",
    "patient_id": "PID-EXAMPLE",
    "body_part": "kidney.left",
    "performed_at": "2026-03-08",
    "measurements": [
      {"item_code": "kidney.ap_diameter", "value": 30, "unit": "mm"}
    ]
  }
]"#;

fn history(raw: &str) -> Vec<RisReport> {
    serde_json::from_str::<Vec<RisReport>>(raw).expect("历史报文应可解析")
}

/// 解析报告正文并取第一条可对拍的条目。
fn first_item() -> ultrasound_report_struct::parse::extract::ParsedItem {
    let out = parse_report(REPORT_TEXT);
    assert_eq!(out.failures.len(), 0, "这份报告不该有失败条目");
    assert_eq!(out.items.len(), 1, "应解析出一条测值");
    out.items.into_iter().next().unwrap()
}

#[test]
fn 整条路径走通_有历史出差值() {
    let item = first_item();
    // 解析层已经把 3.2cm 归一成 32mm。
    assert_eq!(item.part_key.as_deref(), Some("kidney.left"));
    assert_eq!(item.item_code.as_deref(), Some("kidney.ap_diameter"));
    assert_eq!(item.value_si, Some(32.0));
    assert_eq!(item.unit_code.as_deref(), Some("mm"));

    let verdict = judge_item("PID-EXAMPLE", &item, &history(HISTORY_ONE)).expect("应能判定");
    assert_eq!(verdict.state, ComparisonState::HasHistory);
    assert_eq!(verdict.state.as_str(), "has_history");
    assert_eq!(verdict.hit_count, 1);
    let delta = verdict.delta.clone().expect("有历史应给出差值");
    assert_eq!(delta.previous_value, 28.0);
    assert_eq!(delta.current_value, 32.0);
    assert_eq!(delta.delta, 4.0);
    assert_eq!(delta.direction, Direction::Increased);
    assert_eq!(delta.unit_code, "mm");
    // 有历史不是失败。
    assert!(verdict.failure.is_none());
    assert!(verdict.is_consistent());
}

#[test]
fn 无历史判首次且不算失败() {
    let item = first_item();
    let verdict = judge_item("PID-EXAMPLE", &item, &[]).expect("应能判定");
    assert_eq!(verdict.state, ComparisonState::FirstTime);
    assert_eq!(verdict.state.as_str(), "first_time");
    assert_eq!(verdict.hit_count, 0);
    // 首次不是失败：没差值，也没失败条目（失败清单里不该塞满「首次检查」）。
    assert!(verdict.delta.is_none());
    assert!(verdict.failure.is_none());
    assert!(verdict.is_first_time());
    assert!(verdict.is_consistent());
}

#[test]
fn 多历史歧义不产差值并出失败条目() {
    let item = first_item();
    let verdict = judge_item("PID-EXAMPLE", &item, &history(HISTORY_TWO)).expect("应能判定");
    assert_eq!(verdict.state, ComparisonState::Ambiguous);
    assert_eq!(verdict.state.as_str(), "ambiguous");
    assert_eq!(verdict.hit_count, 2);
    // **核心承诺：歧义一律不产差值。**
    assert!(verdict.delta.is_none());
    assert!(!verdict.has_delta());
    // 候选逐条列出 —— 人工据此指定参照哪一份。
    assert_eq!(verdict.candidates.len(), 2);
    assert_eq!(verdict.candidates[0].study_no, "S-HIST-202");
    assert_eq!(verdict.candidates[0].value_si, 28.0);
    assert_eq!(verdict.candidates[1].study_no, "S-HIST-203");
    assert_eq!(verdict.candidates[1].value_si, 30.0);
    // 失败条目：稳定码 + 明细归类 + 处置动作。
    let failure = verdict.failure.as_ref().expect("歧义必须产出失败条目");
    assert_eq!(failure.code, ErrorCode::ComparisonAmbiguous);
    assert_eq!(failure.code.as_str(), "COMPARISON_AMBIGUOUS");
    assert_eq!(
        failure.detail_code(),
        Some(DetailCode::ComparisonMultipleReports)
    );
    assert_eq!(
        failure.disposition(),
        ultrasound_report_struct::parse::errors::Disposition::AwaitingDisambiguation
    );
    assert!(verdict.is_consistent());
}

#[test]
fn 歧义的危险性用数据说清_任取其一会给出不同的差值() {
    // 两条候选差值不同（4mm 与 2mm）：任取其一都会给出一个
    // **看起来正常却可能指错对象**的差值 —— 这正是本层不任取其一的原因。
    let item = first_item();
    let verdict = judge_item("PID-EXAMPLE", &item, &history(HISTORY_TWO)).expect("应能判定");
    assert!(verdict.delta.is_none(), "歧义时不得给出任何差值");

    let mut deltas: Vec<f64> = verdict
        .candidates
        .iter()
        .map(|c| (item.value_si.unwrap() - c.value_si).abs())
        .collect();
    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(deltas, vec![2.0, 4.0], "两条候选会给出两个不同的差值");
}

#[test]
fn 歧义失败条目指得回报告原文() {
    let item = first_item();
    let verdict = judge_item("PID-EXAMPLE", &item, &history(HISTORY_TWO)).expect("应能判定");
    let failure = verdict.failure.as_ref().expect("歧义必须产出失败条目");
    // 位置指回报告里那一条测值（不是 0 —— 位置未知才是 0）。
    assert_eq!(failure.line, item.line);
    assert_eq!(failure.column, item.column);
    assert_eq!(failure.char_start, item.char_start);
    assert!(failure.line > 0);
    // 片段是**原值原单位**（3.2cm），不是归一值（32mm）。
    assert_eq!(failure.raw_snippet.as_str(), "3.2cm");
    // 失败文案说清「差值无法唯一确定」。
    assert!(failure.message().contains("差值无法唯一确定"));
}

#[test]
fn 状态取值与状态库字段一致() {
    // `comparisons.state` 的落库取值就是这三个字符串（跨版本不变）。
    assert_eq!(ComparisonState::HasHistory.as_str(), "has_history");
    assert_eq!(ComparisonState::FirstTime.as_str(), "first_time");
    assert_eq!(ComparisonState::Ambiguous.as_str(), "ambiguous");
    assert_eq!(ComparisonState::lookup("has_history"), Some(ComparisonState::HasHistory));
    assert_eq!(ComparisonState::lookup("first_time"), Some(ComparisonState::FirstTime));
    assert_eq!(ComparisonState::lookup("ambiguous"), Some(ComparisonState::Ambiguous));
    assert_eq!(ComparisonState::lookup("no_such_state"), None);
}
