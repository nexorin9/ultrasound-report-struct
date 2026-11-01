//! 对拍引擎第 2 层：规范键匹配 + 差值与方向计算 —— 集成用例。
//!
//! **这一组用例守什么**：
//!
//! 1. **RIS 报文 → 规范键匹配 → 差值** 是一条完整链路：
//!    历史值不是手工构造的 [`RisReport`]，而是**RIS 会返回的那种 JSON**
//!    （顶层数组、单份报告的顶层对象、数值给数字或给字符串都收）。
//! 2. **手算基准**：32mm vs 28mm 等三组基准，差值与方向逐条断言 ——
//!    差值算错方向会误导医师（「减小」与「增大」的反向错误最致命）。
//! 3. **单位不同先归一再算**：`cm` vs `mm` 混算用例 ——
//!    归一错了会差 10 倍（3.2cm 被当成 3.2mm 参与相减）。
//! 4. **不比的三种情形各有留痕**：不同侧 / 跨量纲族 / 历史值归一不出来 ——
//!    失败可观测，不是静默跳过。

use ultrasound_report_struct::compare::match_history;
use ultrasound_report_struct::compare::{
    ComparisonKey, CurrentMeasure, Direction, RisReport, SkipReason,
};

/// RIS 会返回的那种历史报文（顶层数组）。
const HISTORY_JSON: &str = r#"[
  {
    "study_no": "S-HIST-100",
    "patient_id": "PID-EXAMPLE",
    "body_part": "kidney.left",
    "performed_at": "2026-01-08",
    "measurements": [
      {"item_code": "kidney.ap_diameter", "value": 28, "unit": "mm"},
      {"item_code": "kidney.length", "value": 85, "unit": "mm"}
    ]
  },
  {
    "study_no": "S-HIST-101",
    "patient_id": "PID-EXAMPLE",
    "body_part": "kidney.left",
    "performed_at": "2026-03-08",
    "measurements": [
      {"item_code": "kidney.ap_diameter", "value": "2.8", "unit": "cm"}
    ]
  },
  {
    "study_no": "S-HIST-102",
    "patient_id": "PID-EXAMPLE",
    "body_part": "kidney.right",
    "performed_at": "2026-02-08",
    "measurements": [
      {"item_code": "kidney.ap_diameter", "value": 25, "unit": "mm"}
    ]
  },
  {
    "study_no": "S-HIST-103",
    "patient_id": "PID-OTHER",
    "body_part": "kidney.left",
    "performed_at": "2026-02-08",
    "measurements": [
      {"item_code": "kidney.ap_diameter", "value": 19, "unit": "mm"}
    ]
  }
]"#;

fn history() -> Vec<RisReport> {
    serde_json::from_str::<Vec<RisReport>>(HISTORY_JSON).expect("历史报文应可解析")
}

fn key() -> ComparisonKey {
    ComparisonKey::new("PID-EXAMPLE", "kidney.left", "kidney.ap_diameter")
}

fn current(value: f64, unit: &str) -> CurrentMeasure {
    CurrentMeasure {
        value_si: Some(value),
        unit_code: Some(unit.to_string()),
    }
}

#[test]
fn ris报文走通规范键匹配到差值() {
    // 当前 32mm；同侧同测项的历史命中两条（28mm 与 2.8cm）→ **歧义，不给差值**。
    let outcome = match_history(&key(), &current(32.0, "mm"), &history());
    assert_eq!(outcome.hit_count(), 2);
    assert!(outcome.is_ambiguous());
    assert!(outcome.delta.is_none());
    // 两条命中的原值原单位都留着，便于回溯「上次那边写的是 2.8cm」。
    let units: Vec<&str> = outcome
        .hits
        .iter()
        .map(|h| h.unit_raw.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(units, vec!["mm", "cm"]);
    // 不同侧与不同患者的历史都被排除并留原因。
    // **「左右不同」有独立的原因码**（Task 31 的同侧规则表）——
    // 与「不是同一个器官」分开，运维按码聚合时能分清该改哪一头。
    assert_eq!(outcome.skipped_count(SkipReason::SideMismatch), 1);
    assert_eq!(outcome.skipped_count(SkipReason::BodyPartMismatch), 0);
    assert_eq!(outcome.skipped_count(SkipReason::PatientMismatch), 1);
    // 同一份报告里的别的测项（肾长）不算命中。
    assert_eq!(outcome.skipped_count(SkipReason::ItemMismatch), 1);
}

#[test]
fn 三组手算基准差值与方向正确() {
    // 只留一份历史时，逐条对拍手算基准。
    let one: Vec<RisReport> = vec![history()[0].clone()];
    let delta = match_history(&key(), &current(32.0, "mm"), &one).delta.unwrap();
    assert_eq!(delta.previous_value, 28.0);
    assert_eq!(delta.current_value, 32.0);
    assert_eq!(delta.delta, 4.0);
    assert_eq!(delta.direction, Direction::Increased);

    let delta = match_history(&key(), &current(25.0, "mm"), &one).delta.unwrap();
    assert_eq!(delta.delta, -3.0);
    assert_eq!(delta.direction, Direction::Decreased);

    let delta = match_history(&key(), &current(28.0, "mm"), &one).delta.unwrap();
    assert_eq!(delta.delta, 0.0);
    assert_eq!(delta.direction, Direction::Unchanged);
}

#[test]
fn 单位不同先归一再算差值() {
    // 历史给 2.8cm（第二份报告），当前 32mm → 归一后 28mm，差 4mm。
    let cm_history: Vec<RisReport> = vec![history()[1].clone()];
    let delta = match_history(&key(), &current(32.0, "mm"), &cm_history)
        .delta
        .unwrap();
    assert_eq!(delta.previous_value, 28.0);
    assert_eq!(delta.delta, 4.0);
    assert_eq!(delta.direction, Direction::Increased);
    assert_eq!(delta.unit_code, "mm");

    // 历史值单位未知 → 归一不出来 → 不比（不是「按 mm 兜底」）。
    let bad = serde_json::from_str::<Vec<RisReport>>(
        r#"[{"study_no":"S-HIST-104","patient_id":"PID-EXAMPLE","body_part":"kidney.left",
             "measurements":[{"item_code":"kidney.ap_diameter","value":28,"unit":"xx"}]}]"#,
    )
    .unwrap();
    let outcome = match_history(&key(), &current(32.0, "mm"), &bad);
    assert!(outcome.delta.is_none());
    assert_eq!(
        outcome.skipped_count(SkipReason::HistoryValueNotCoercible),
        1
    );
}

#[test]
fn 跨量纲族不相减且留痕() {
    // 历史给毫升（残余尿量那种），当前是毫米 —— 不硬减。
    let ml = serde_json::from_str::<Vec<RisReport>>(
        r#"[{"study_no":"S-HIST-105","patient_id":"PID-EXAMPLE","body_part":"kidney.left",
             "measurements":[{"item_code":"kidney.ap_diameter","value":28,"unit":"ml"}]}]"#,
    )
    .unwrap();
    let outcome = match_history(&key(), &current(32.0, "mm"), &ml);
    assert!(outcome.delta.is_none());
    assert_eq!(outcome.skipped_count(SkipReason::FamilyMismatch), 1);
}

#[test]
fn 命中零条是无历史不产差值() {
    let empty: Vec<RisReport> = Vec::new();
    let outcome = match_history(&key(), &current(32.0, "mm"), &empty);
    assert_eq!(outcome.hit_count(), 0);
    assert!(!outcome.is_ambiguous());
    assert!(!outcome.is_single_hit());
    assert!(outcome.delta.is_none());
}

#[test]
fn 顶层单对象的历史报文也收() {
    // 院内 RIS 的这一版接口两种形状都见过（见 ris_client 的契约用例）。
    let single: Vec<RisReport> = serde_json::from_str::<Vec<RisReport>>(
        r#"[{"study_no":"S-HIST-106","patient_id":"PID-EXAMPLE","body_part":"kidney.left",
             "performed_at":"2026-01-08",
             "measurements":[{"item_code":"kidney.ap_diameter","value":28,"unit":"mm"}]}]"#,
    )
    .unwrap();
    let delta = match_history(&key(), &current(32.0, "mm"), &single)
        .delta
        .unwrap();
    assert_eq!(delta.delta, 4.0);
    assert_eq!(delta.direction, Direction::Increased);
}
