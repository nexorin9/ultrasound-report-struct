//! 对拍引擎第 4 层（三）：**一轮对拍** —— 固定取数时点 + 上游不可达降级。
//!
//! **这一层解决的接缝**：对拍要连着问三个问题 ——
//!
//! 1. **这次比的是不是同一侧？** → 侧别规则表（[`crate::compare::side`]);
//! 2. **参照基线是哪一刻的？** → 固定取数时点（[`crate::compare::snapshot`]);
//! 3. **RIS 够不着的时候怎么办？** → 显式降级，**绝不当作「无差异」**。
//!
//! **一轮对拍 = 一份报告的一次对拍**：一个取数时点、一份历史快照，
//! 这一轮里所有测值**共用同一把尺子**。这解决了「每条测值各取一次历史」
//! 带来的「三条差值不是同一把尺子量的」问题。
//!
//! **RIS 不可达降级（本层的核心承诺）**：
//!
//! | 情形 | 状态 | 差值 | 失败条目 |
//! |---|---|---|---|
//! | RIS 连不上 / 超时 / 5xx | `comparison_skipped` | **不产出** | `RIS_UPSTREAM_UNAVAILABLE` |
//! | RIS 401 / 403 | `comparison_skipped` | **不产出** | `RIS_AUTH_FAILED` |
//! | RIS 响应不合契约 | `comparison_skipped` | **不产出** | `RIS_BAD_RESPONSE` |
//! | RIS 端点或凭据缺失 | `comparison_skipped` | **不产出** | `RIS_NOT_CONFIGURED` |
//!
//! **为什么必须显式跳过、而不是当成「没有历史」**：
//! 「首次检查」与「没比成」在医师眼里是两件事 ——
//! 前者是**正常业务结果**，后者是**系统没做成**。
//! 混成一样，医师会以为「跟上次一样」，而其实**压根没比成**。
//! 所以降级落 `comparison_skipped` 这个**独立状态**，并带上游错误码 ——
//! 运维一眼分得清「该补词典」还是「该查 RIS」。
//!
//! **报告本身不受影响**：RIS 够不着只是**对不上历史**，
//! 测值照样落台账、照样带数值、照样等医师确认。

use crate::compare::ris_client::{RisError, RisReport};
use crate::compare::snapshot::{detect_drift, HistorySnapshot, SnapshotDrift};
use crate::compare::states::{judge_item, ComparisonVerdict, MeasureAnchor};
use crate::parse::extract::ParsedItem;

/// **一轮对拍**：固定取数时点 + 固定历史快照 + 降级状态。
#[derive(Clone, Debug)]
pub struct ComparisonRound {
    /// 本轮使用的历史快照（**固定**，整轮不变）。
    snapshot: HistorySnapshot,
    /// 上游失败（RIS 够不着时的原因；`None` 表示取数成功）。
    degradation: Option<RisError>,
    /// 快照漂移（取数期间历史变过 —— 留痕，不改差值）。
    drift: Option<SnapshotDrift>,
}

impl ComparisonRound {
    /// 用取到的历史开一轮对拍。
    pub fn from_reports(reports: Vec<RisReport>) -> ComparisonRound {
        ComparisonRound {
            snapshot: HistorySnapshot::capture(reports),
            degradation: None,
            drift: None,
        }
    }

    /// 用**指定时点**的历史开一轮对拍（便于复现与回归）。
    pub fn from_reports_at(reports: Vec<RisReport>, unix_secs: u64) -> ComparisonRound {
        ComparisonRound {
            snapshot: HistorySnapshot::capture_at(reports, unix_secs),
            degradation: None,
            drift: None,
        }
    }

    /// 用**上游失败**开一轮对拍（RIS 够不着）。
    ///
    /// 快照是**空的** —— 没取到历史就没有参照基线；
    /// 本轮所有测值都判成 `comparison_skipped`。
    pub fn from_ris_error(err: &RisError) -> ComparisonRound {
        ComparisonRound {
            snapshot: HistorySnapshot::capture_at(Vec::new(), crate::state::utc_now_secs()),
            degradation: Some(err.clone()),
            drift: None,
        }
    }

    /// 本轮的历史快照。
    pub fn snapshot(&self) -> &HistorySnapshot {
        &self.snapshot
    }

    /// 本轮是否处于降级状态（RIS 够不着）。
    pub fn is_degraded(&self) -> bool {
        self.degradation.is_some()
    }

    /// 降级原因（RIS 失败详情）。
    pub fn degradation(&self) -> Option<&RisError> {
        self.degradation.as_ref()
    }

    /// 快照漂移（取数期间历史变过）。
    pub fn drift(&self) -> Option<&SnapshotDrift> {
        self.drift.as_ref()
    }

    /// **一致性复核**：拿另一份快照比对，变了就**标记漂移**。
    ///
    /// 返回 `true` 表示检出漂移。漂移**不改差值** —— 差值是对固定快照算的，
    /// 那是可核对依据；漂移是留痕，让运维知道 RIS 侧动过。
    pub fn verify_against(&mut self, later: &HistorySnapshot) -> bool {
        match detect_drift(&self.snapshot, later) {
            Some(drift) => {
                self.drift = Some(drift);
                true
            }
            None => false,
        }
    }

    /// 手工挂上漂移（由调用方从别处拿到漂移证据时用）。
    pub fn attach_drift(&mut self, drift: SnapshotDrift) {
        self.drift = Some(drift);
    }

    /// 对一条测值做对拍。
    ///
    /// - **降级状态**（RIS 够不着）→ 返回 `comparison_skipped` 结论：
    ///   不产差值、带上游错误码；
    /// - **正常状态** → 走三态判定（有历史 / 无历史 / 多历史歧义）。
    ///
    /// 部位键或测项码缺失时返回 `None`（键不全就没有可比对象）。
    pub fn compare_item(&self, patient_id: &str, item: &ParsedItem) -> Option<ComparisonVerdict> {
        if let Some(err) = &self.degradation {
            // 上游不可达 → 显式跳过：不是「首次」，是「没比成」。
            return Some(self.skipped_verdict(patient_id, err, item));
        }
        let mut verdict = judge_item(patient_id, item, self.snapshot.reports())?;
        // 快照漂移随结论一起带出去（留痕的载体）。
        verdict.snapshot_drift = self.drift.clone();
        Some(verdict)
    }

    /// 降级状态下的结论：`comparison_skipped` + 上游错误码。
    fn skipped_verdict(
        &self,
        patient_id: &str,
        err: &RisError,
        item: &ParsedItem,
    ) -> ComparisonVerdict {
        let mut verdict = skipped_verdict_for(patient_id, err, item);
        verdict.snapshot_drift = self.drift.clone();
        verdict
    }
}

/// 降级状态下的结论（**不进对拍、不产差值、带上游错误码**）。
///
/// 单列一个函数，便于调用方与测试走同一条路径 ——
/// 「RIS 够不着」在产品里只有这一个出口。
pub fn skipped_verdict(err: &RisError, item: &ParsedItem) -> ComparisonVerdict {
    skipped_verdict_for("", err, item)
}

/// 降级状态下的结论（**带患者主索引**的规范键 + 原文锚点）。
///
/// **锚点**：上游错误本身不在报告正文里（位置 0），
/// 但**没比成的那条测值**在正文里有位置 —— 调用方据此告诉用户
/// 「是哪一条没比成」。
pub fn skipped_verdict_for(
    patient_id: &str,
    err: &RisError,
    item: &ParsedItem,
) -> ComparisonVerdict {
    let key = crate::compare::r#match::ComparisonKey::from_item(patient_id, item)
        .unwrap_or_else(|| crate::compare::r#match::ComparisonKey::new(patient_id, "", ""));
    let mut verdict = ComparisonVerdict::skipped(key, err.to_failure());
    verdict.anchor = MeasureAnchor::from_item(item);
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::ris_client::{RisError, RisErrorKind, RisReportMeasurement};
    use crate::parse::errors::ErrorCode;

    fn report(study_no: &str, value: f64) -> RisReport {
        RisReport {
            study_no: study_no.to_string(),
            patient_id: Some("PID-EXAMPLE".to_string()),
            body_part: Some("kidney.left".to_string()),
            performed_at: Some("2026-01-01".to_string()),
            measurements: vec![RisReportMeasurement {
                item_code: "kidney.ap_diameter".to_string(),
                value: Some(value),
                unit: Some("mm".to_string()),
            }],
        }
    }

    fn item() -> ParsedItem {
        ParsedItem {
            part_key: Some("kidney.left".to_string()),
            item_code: Some("kidney.ap_diameter".to_string()),
            value_raw: Some(3.2),
            unit_raw: Some("cm".to_string()),
            value_si: Some(32.0),
            unit_code: Some("mm".to_string()),
            coercion_path: crate::coerce::path::CoercionPath::UnitConvert,
            line: 2,
            column: 6,
            char_start: 12,
            notes: Vec::new(),
            dictionary_version: crate::dictionary_version(),
        }
    }

    #[test]
    fn 上游不可达时判跳过且不产差值() {
        let err = RisError::new(RisErrorKind::UpstreamUnavailable, None, "RIS 连不上");
        let round = ComparisonRound::from_ris_error(&err);
        assert!(round.is_degraded());
        let verdict = round.compare_item("PID-EXAMPLE", &item()).expect("降级也要给出结论");
        assert_eq!(verdict.state.as_str(), "comparison_skipped");
        assert!(verdict.delta.is_none());
        let failure = verdict.failure.as_ref().expect("降级必须带失败条目");
        assert_eq!(failure.code, ErrorCode::RisUpstreamUnavailable);
    }

    #[test]
    fn 降级结论不是首次() {
        // 「没比成」绝不冒充「首次检查」—— 那是两件不同的事。
        let err = RisError::new(RisErrorKind::UpstreamUnavailable, None, "RIS 连不上");
        let round = ComparisonRound::from_ris_error(&err);
        let verdict = round.compare_item("PID-EXAMPLE", &item()).expect("降级也要给出结论");
        assert!(!verdict.is_first_time());
        assert_eq!(verdict.hit_count, 0);
        assert!(verdict.is_consistent());
    }

    #[test]
    fn 正常轮次走三态判定() {
        let round = ComparisonRound::from_reports_at(vec![report("S-1", 28.0)], 1_700_000_000);
        assert!(!round.is_degraded());
        let verdict = round.compare_item("PID-EXAMPLE", &item()).expect("应能判定");
        assert_eq!(verdict.state.as_str(), "has_history");
        assert_eq!(verdict.delta.expect("有历史应给出差值").delta, 4.0);
    }

    #[test]
    fn 同一轮里所有测值共用一份快照() {
        let round = ComparisonRound::from_reports_at(vec![report("S-1", 28.0)], 1_700_000_000);
        assert_eq!(round.snapshot().len(), 1);
        assert_eq!(round.snapshot().digest(), round.snapshot().digest());
        assert!(!round.snapshot().taken_at().is_empty());
    }

    #[test]
    fn 复核检出漂移并留痕但不改差值() {
        let mut round = ComparisonRound::from_reports_at(vec![report("S-1", 28.0)], 1_700_000_000);
        let verdict_before = round.compare_item("PID-EXAMPLE", &item()).expect("应能判定");
        let delta_before = verdict_before.delta.expect("有历史应给出差值").delta;

        let later = HistorySnapshot::capture_at(vec![report("S-1", 31.0)], 1_700_000_600);
        assert!(round.verify_against(&later), "应检出快照漂移");
        let drift = round.drift().expect("漂移应留痕");
        assert_eq!(drift.change_code, SnapshotDrift::CHANGE_CODE);
        assert_eq!(drift.changed_studies, vec!["S-1".to_string()]);

        // 差值仍按**固定快照**算 —— 不因漂移而悄悄改数。
        let verdict_after = round.compare_item("PID-EXAMPLE", &item()).expect("应能判定");
        assert_eq!(verdict_after.delta.expect("仍应给出差值").delta, delta_before);
        assert!(verdict_after.snapshot_drift.is_some(), "漂移应随结论带出去");
    }

    #[test]
    fn 复核未变时不留痕() {
        let mut round = ComparisonRound::from_reports_at(vec![report("S-1", 28.0)], 1_700_000_000);
        let later = HistorySnapshot::capture_at(vec![report("S-1", 28.0)], 1_700_000_600);
        assert!(!round.verify_against(&later));
        assert!(round.drift().is_none());
    }
}
