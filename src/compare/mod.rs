//! 对拍引擎：RIS 历史取数、规范键匹配、差值与三态判定。
//!
//! **失败口径（本层与解析层共用一张码表）**：对拍层**不自造错误码、不自定处置规则** ——
//! 失败一律走 [`crate::parse::errors`] 的稳定码表（`ErrorCode` / `DetailCode` / `Failure`），
//! 「这条测值还进不进对拍」由码表的策略决定
//! （[`crate::parse::errors::FailurePolicy::excluded_from_comparison`]）。
//! 解析层与对拍层共用一张处置口径，运维看失败清单时不用记两套规则。
//!
//! **fail-closed**：对不上的测值宁可**不参与**对拍 ——
//! 部位 / 测项键不全、或归一值算不出来时，硬算出来的差值**看着正常却可能指错对象**，
//! 那比「没有差值」危险得多。
//!
//! **本模块当前落地的是「候选筛选」「规范键匹配 + 差值」「三态判定」与
//! 「同侧规则 + 快照一致性 + 上游降级」**：
//! 把一份报告里**能进对拍**的测值挑出来并说明每条被排除的测值挂在哪个失败码上；
//! 再按 **(patient_id, part_key, item_code)** 规范键与 RIS 历史值对上，
//! 归一后算出差值与方向（增大 / 减小 / 持平）；
//! 然后判出这轮对拍**算没算成**（有历史 / 无历史 / 多历史歧义）——
//! 歧义时**绝不任取其一**，产出失败条目交人工判定；
//! 最后按**同侧规则表**判「这两个部位能不能放在一起比」，
//! 并把**一轮对拍固定在一个取数时点**上；RIS 够不着时显式落
//! `comparison_skipped`，**绝不当作「无差异」**。

use crate::parse::errors::{ErrorCode, Failure};
use crate::parse::extract::ParsedItem;

pub mod ris_client;

/// 规范键匹配 + 差值与方向计算。
///
/// **模块名带 `r#`** —— `match` 是 Rust 关键字，不能直接当模块名；
/// 文件名仍是 `match.rs`（任务约定的路径），调用方用本模块导出的
/// [`match_history`] 等名字，不必写 `r#match::`。
pub mod r#match;

/// 三态判定（有历史 / 无历史 / 多历史歧义）+ 降级状态（`comparison_skipped`）。
pub mod states;

/// **同侧判定规则表**（双侧 / 单侧 / 未标注侧别的可比性）。
pub mod side;

/// **历史快照一致性**（固定取数时点 + 漂移检测）。
pub mod snapshot;

/// **一轮对拍**（固定快照 + 上游不可达降级）。
pub mod round;

/// **差值基准回归集**（oracle = RIS 真实历史值；差值 / 方向 / 三态 / 降级逐条断言）。
pub mod baseline;

/// **对拍入账**（台账行 + RIS 历史 → `comparisons` 差值行 + 留痕 + 进确认队列）。
pub mod pipeline;

pub use baseline::{
    report as baseline_report, BaselineCase, BaselineError, BaselineOutcome, BaselineTable,
    HistoryEntry, Mismatch as BaselineMismatch, MIN_ROWS as BASELINE_MIN_ROWS,
};
pub use r#match::{
    match_history, match_item, ComparisonKey, CurrentMeasure, Delta, Direction, HistoryHit,
    MatchOutcome, SkipReason, SkippedHistory,
};
pub use ris_client::{
    classify_status, classify_transport, RisClient, RisError, RisErrorKind, RisQuery, RisReport,
    RisReportMeasurement, DEFAULT_MAX_RETRIES, RIS_HISTORY_PATH,
};
pub use round::{skipped_verdict, ComparisonRound};
pub use side::{base_of, compare_sides, laterality_of, Laterality, SideComparability, SideMismatch};
pub use snapshot::{detect_drift, HistorySnapshot, SnapshotDrift};
pub use states::{
    judge, judge_history, judge_item, ComparisonState, ComparisonVerdict, MeasureAnchor,
};

/// 一次筛选的结果：可对拍的测值 + 被排除的测值及原因码。
#[derive(Clone, Debug, Default)]
pub struct CompareSelection<'a> {
    /// 可进入对拍的测值。
    pub comparable: Vec<&'a ParsedItem>,
    /// 被排除的测值（连同原因码，便于运维对账）。
    pub excluded: Vec<(&'a ParsedItem, ErrorCode)>,
}

/// 报告级阻断：任一失败码的策略是「阻断整份报告」→ 整份不进对拍。
///
/// 返回那个阻断码（多个时取**第一个**—— 报告级问题先到先算）。
pub fn report_blocked(failures: &[Failure]) -> Option<ErrorCode> {
    failures
        .iter()
        .find(|f| f.code.policy().blocks_report)
        .map(|f| f.code)
}

/// 这条测值是否进入对拍（口径取自码表策略，本模块不另定规则）。
///
/// 三条口径，逐条可断言：
///
/// 1. **挂了失败码** → 不进（失败码的策略说「排除在对拍之外」就排除）；
/// 2. **备注里有查不到的码** → 不进（码表里没有的码说明口径漂了，宁可不比不错比）；
/// 3. **没有归一值** → 不进（`value_si` 为空的测值对不了历史值）。
pub fn participates_in_comparison(item: &ParsedItem) -> bool {
    if item.value_si.is_none() {
        return false;
    }
    for note in &item.notes {
        match ErrorCode::lookup(note) {
            Some(code) => {
                if code.policy().excluded_from_comparison {
                    return false;
                }
            }
            // 码表里没有的备注：口径漂了 → 不进对拍（fail-closed）。
            None => return false,
        }
    }
    true
}

/// 按码表策略筛出可进入对拍的测值。
///
/// 报告级有阻断失败时，整份不进对拍（返回空的可对拍列表）。
pub fn select_comparable<'a>(
    items: &'a [ParsedItem],
    failures: &[Failure],
) -> CompareSelection<'a> {
    let mut selection = CompareSelection::default();
    if let Some(blocking) = report_blocked(failures) {
        selection.excluded = items.iter().map(|i| (i, blocking)).collect();
        return selection;
    }
    for item in items {
        if participates_in_comparison(item) {
            selection.comparable.push(item);
        } else {
            let code = item
                .notes
                .iter()
                .find_map(|n| ErrorCode::lookup(n))
                .unwrap_or(ErrorCode::CoercionNoPath);
            selection.excluded.push((item, code));
        }
    }
    selection
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coerce::path::CoercionPath;
    use crate::parse::errors::FailurePolicy;
    use crate::parse::extract::ParsedItem;

    fn item(notes: Vec<&str>, value_si: Option<f64>) -> ParsedItem {
        ParsedItem {
            part_key: Some("kidney.left".to_string()),
            item_code: Some("kidney.ap_diameter".to_string()),
            value_raw: Some(3.2),
            unit_raw: Some("cm".to_string()),
            value_si,
            unit_code: value_si.map(|_| "mm".to_string()),
            coercion_path: CoercionPath::UnitConvert,
            line: 1,
            column: 6,
            char_start: 5,
            notes: notes.into_iter().map(|s| s.to_string()).collect(),
            dictionary_version: crate::dictionary_version(),
        }
    }

    #[test]
    fn 干净测值进对拍() {
        let item = item(vec![], Some(32.0));
        assert!(participates_in_comparison(&item));
    }

    #[test]
    fn 部位未命中的测值不进对拍() {
        let item = item(vec![ErrorCode::PartUnresolved.as_str()], Some(32.0));
        assert!(!participates_in_comparison(&item));
    }

    #[test]
    fn 没有归一值的测值不进对拍() {
        let item = item(vec![], None);
        assert!(!participates_in_comparison(&item));
    }

    #[test]
    fn 备注里有表外码不进对拍() {
        let item = item(vec!["SOME_FREE_TEXT"], Some(32.0));
        assert!(!participates_in_comparison(&item));
    }

    #[test]
    fn 报告级阻断时整份不进对拍() {
        let items = vec![item(vec![], Some(32.0))];
        let failures = vec![Failure::new(ErrorCode::EncodingMixed, 0, 0, 0, "混编")];
        assert_eq!(report_blocked(&failures), Some(ErrorCode::EncodingMixed));
        let selection = select_comparable(&items, &failures);
        assert!(selection.comparable.is_empty());
    }

    #[test]
    fn 筛选结果带上被排除的原因码() {
        let items = vec![
            item(vec![], Some(32.0)),
            item(vec![ErrorCode::ItemDictMissing.as_str()], Some(21.0)),
        ];
        let selection = select_comparable(&items, &[]);
        assert_eq!(selection.comparable.len(), 1);
        assert_eq!(selection.excluded.len(), 1);
        assert_eq!(selection.excluded[0].1, ErrorCode::ItemDictMissing);
        let policy: FailurePolicy = selection.excluded[0].1.policy();
        assert!(policy.excluded_from_comparison);
    }
}

/// **Task 31 验收**：同侧判定 + 快照一致性 + RIS 不可达降级。
///
/// 三组断言合起来回答一句话 ——
/// **「这个差值是不是可信的」**：
///
/// 1. **同侧可比性逐条断言** —— 双侧 vs 单侧不可比、左右不同不可比、
///    侧别未标注不可比、单器官可比；
/// 2. **快照一致性** —— 一轮对拍固定一个取数时点，取数期间历史变过要留痕；
/// 3. **RIS 不可达降级** —— 落 `comparison_skipped`、**不产差值**、
///    **不冒充「首次检查」**。
#[cfg(test)]
mod side_and_degrade {
    use super::*;
    use crate::compare::ris_client::{RisError, RisErrorKind, RisReportMeasurement};
    use crate::parse::errors::ErrorCode;

    /// 造一份历史报告。
    fn report(study_no: &str, body_part: &str, value: f64) -> RisReport {
        RisReport {
            study_no: study_no.to_string(),
            patient_id: Some("PID-EXAMPLE".to_string()),
            body_part: Some(body_part.to_string()),
            performed_at: Some("2026-01-01".to_string()),
            measurements: vec![RisReportMeasurement {
                item_code: "kidney.ap_diameter".to_string(),
                value: Some(value),
                unit: Some("mm".to_string()),
            }],
        }
    }

    /// 当前测值（左肾前后径 3.2cm → 32mm）。
    fn current_item() -> ParsedItem {
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
    fn 同侧可比性逐条断言() {
        // 同侧可比。
        assert!(compare_sides("kidney.left", "kidney.left").is_comparable());
        assert!(compare_sides("kidney.right", "kidney.right").is_comparable());
        assert!(compare_sides("kidney.bilateral", "kidney.bilateral").is_comparable());
        // 单器官可比（膀胱只有一个，侧别这回事不存在）。
        assert!(compare_sides("bladder", "bladder").is_comparable());
        // 左右不同不可比。
        assert_eq!(
            compare_sides("kidney.left", "kidney.right").mismatch(),
            Some(SideMismatch::DifferentSide)
        );
        // 双侧 vs 单侧不可比。
        assert_eq!(
            compare_sides("kidney.bilateral", "kidney.left").mismatch(),
            Some(SideMismatch::BilateralVsUnilateral)
        );
        assert_eq!(
            compare_sides("kidney.left", "kidney.bilateral").mismatch(),
            Some(SideMismatch::BilateralVsUnilateral)
        );
        // 侧别未标注不可比（连同为未标注也不可比）。
        assert_eq!(
            compare_sides("kidney", "kidney").mismatch(),
            Some(SideMismatch::SideUnspecified)
        );
        assert_eq!(
            compare_sides("kidney", "kidney.left").mismatch(),
            Some(SideMismatch::SideUnspecified)
        );
        // 不是同一个器官不可比。
        assert_eq!(
            compare_sides("kidney.left", "bladder").mismatch(),
            Some(SideMismatch::DifferentBase)
        );
        // 每条不可比原因都有中文说明与稳定字符串。
        for reason in SideMismatch::ALL {
            assert!(!reason.message().is_empty());
            assert!(!reason.as_str().is_empty());
        }
    }

    #[test]
    fn 匹配层按侧别规则表给出独立排除原因() {
        // 左 vs 右 → 拿错侧；双侧 vs 单侧 → 口径不可比；膀胱 vs 肾 → 不是同一器官。
        let history = vec![
            report("S-R", "kidney.right", 28.0),
            report("S-B", "kidney.bilateral", 27.0),
            report("S-L", "bladder", 26.0),
        ];
        let key = ComparisonKey::new("PID-EXAMPLE", "kidney.left", "kidney.ap_diameter");
        let current = CurrentMeasure {
            value_si: Some(32.0),
            unit_code: Some("mm".to_string()),
        };
        let outcome = match_history(&key, &current, &history);
        assert_eq!(outcome.hit_count(), 0);
        assert!(outcome.delta.is_none());
        assert_eq!(outcome.skipped_count(SkipReason::SideMismatch), 1);
        assert_eq!(outcome.skipped_count(SkipReason::BilateralVsUnilateral), 1);
        assert_eq!(outcome.skipped_count(SkipReason::BodyPartMismatch), 1);
    }

    #[test]
    fn 同侧历史能对上并给出差值() {
        // 规则表放行的那条路要真的走得通 —— 不是只判不可比。
        let history = vec![report("S-L", "kidney.left", 28.0)];
        let key = ComparisonKey::new("PID-EXAMPLE", "kidney.left", "kidney.ap_diameter");
        let current = CurrentMeasure {
            value_si: Some(32.0),
            unit_code: Some("mm".to_string()),
        };
        let outcome = match_history(&key, &current, &history);
        assert_eq!(outcome.hit_count(), 1);
        assert_eq!(outcome.delta.expect("同侧应给出差值").delta, 4.0);
    }

    #[test]
    fn 一轮对拍固定一个取数时点() {
        let round = ComparisonRound::from_reports_at(vec![report("S-L", "kidney.left", 28.0)], 1_700_000_000);
        // 同一轮里快照不随调用变。
        let first = round.snapshot().digest().to_string();
        let verdict = round.compare_item("PID-EXAMPLE", &current_item()).expect("应能判定");
        assert_eq!(round.snapshot().digest(), first);
        assert_eq!(round.snapshot().taken_at(), "2023-11-14 22:13:20");
        assert_eq!(verdict.state, ComparisonState::HasHistory);
    }

    #[test]
    fn 取数期间历史变了要留痕但不改差值() {
        let mut round = ComparisonRound::from_reports_at(vec![report("S-L", "kidney.left", 28.0)], 1_700_000_000);
        let before = round
            .compare_item("PID-EXAMPLE", &current_item())
            .expect("应能判定")
            .delta
            .expect("有历史应给出差值")
            .delta;
        assert_eq!(before, 4.0);

        let later = HistorySnapshot::capture_at(vec![report("S-L", "kidney.left", 30.0)], 1_700_000_600);
        assert!(round.verify_against(&later), "应检出快照变化");

        let verdict = round.compare_item("PID-EXAMPLE", &current_item()).expect("应能判定");
        // 差值仍按**固定快照**算 —— 不因漂移而悄悄改数。
        assert_eq!(verdict.delta.expect("仍应给出差值").delta, before);
        // 但漂移要看得见（留痕的载体随结论带出去）。
        let drift = verdict.snapshot_drift.as_ref().expect("漂移应随结论带出去");
        assert_eq!(drift.change_code, SnapshotDrift::CHANGE_CODE);
        assert_eq!(drift.changed_studies, vec!["S-L".to_string()]);
        assert!(drift.message().contains("取数期间历史发生变化"));
    }

    #[test]
    fn ris不可达时落跳过且不产差值() {
        // 上游不可达 → 显式跳过：不产差值、不当作无差异。
        let err = RisError::new(
            RisErrorKind::UpstreamUnavailable,
            None,
            "RIS 请求未完成（连接被拒）",
        );
        let round = ComparisonRound::from_ris_error(&err);
        assert!(round.is_degraded());
        let verdict = round.compare_item("PID-EXAMPLE", &current_item()).expect("降级也要给出结论");
        assert_eq!(verdict.state, ComparisonState::Skipped);
        assert_eq!(verdict.state.as_str(), "comparison_skipped");
        assert!(verdict.delta.is_none());
        assert!(!verdict.has_delta());
        assert_eq!(verdict.hit_count, 0);
        // 带上游错误码 —— 运维一眼分得清是「RIS 的问题」不是「词典的问题」。
        let failure = verdict.failure.as_ref().expect("降级必须带失败条目");
        assert_eq!(failure.code, ErrorCode::RisUpstreamUnavailable);
        assert_eq!(failure.code.as_str(), "RIS_UPSTREAM_UNAVAILABLE");
        // 处置动作是「待上游恢复」。
        assert_eq!(
            failure.disposition(),
            crate::parse::errors::Disposition::AwaitingUpstream
        );
        assert!(verdict.is_consistent());
    }

    #[test]
    fn ris四类失败各自落跳过且不产差值() {
        // 四类出站失败语义（上游不可达 / 鉴权 / 响应不合契约 / 未配置）
        // **都**走同一条降级路径 —— 没有一条会被当成「首次检查」。
        let cases = [
            (
                RisErrorKind::UpstreamUnavailable,
                ErrorCode::RisUpstreamUnavailable,
            ),
            (RisErrorKind::AuthFailed, ErrorCode::RisAuthFailed),
            (RisErrorKind::BadResponse, ErrorCode::RisBadResponse),
            (RisErrorKind::NotConfigured, ErrorCode::RisNotConfigured),
        ];
        for (kind, expected_code) in cases {
            let err = RisError::new(kind, None, "出站读失败");
            let round = ComparisonRound::from_ris_error(&err);
            let verdict = round
                .compare_item("PID-EXAMPLE", &current_item())
                .expect("降级也要给出结论");
            assert_eq!(verdict.state, ComparisonState::Skipped);
            assert!(verdict.delta.is_none());
            assert_eq!(
                verdict.failure.as_ref().expect("降级必须带失败条目").code,
                expected_code
            );
        }
    }

    #[test]
    fn 跳过绝不冒充首次检查() {
        // 「首次」是正常业务结果，「跳过」是系统没做成 —— 两件事不能混。
        let err = RisError::new(RisErrorKind::UpstreamUnavailable, None, "RIS 连不上");
        let round = ComparisonRound::from_ris_error(&err);
        let verdict = round.compare_item("PID-EXAMPLE", &current_item()).expect("降级也要给出结论");
        assert!(!verdict.is_first_time());
        assert!(verdict.is_skipped());
        assert_ne!(verdict.state, ComparisonState::FirstTime);
        // 首次状态是「零命中、无差值、无失败条目」—— 与跳过在结构上就不同。
        let first_time = judge(
            &ComparisonKey::new("PID-EXAMPLE", "kidney.left", "kidney.ap_diameter"),
            &CurrentMeasure {
                value_si: Some(32.0),
                unit_code: Some("mm".to_string()),
            },
            &[],
        )
        .expect("当前测值可用应能判定");
        assert!(first_time.is_first_time());
        assert!(first_time.failure.is_none());
        assert!(verdict.failure.is_some());
    }

    #[test]
    fn 降级时测值本身不受影响() {
        // RIS 够不着只是「对不上历史」—— 测值照样解析、照样带数值。
        let item = current_item();
        assert_eq!(item.value_si, Some(32.0));
        assert_eq!(item.unit_code.as_deref(), Some("mm"));
        let err = RisError::new(RisErrorKind::UpstreamUnavailable, None, "RIS 连不上");
        let round = ComparisonRound::from_ris_error(&err);
        let verdict = round.compare_item("PID-EXAMPLE", &item).expect("降级也要给出结论");
        // 结论里仍带着这条测值的锚点（知道是哪一条没比成）。
        assert_eq!(verdict.anchor.line, 2);
        assert_eq!(verdict.anchor.column, 6);
        assert_eq!(verdict.anchor.raw_text.as_str(), "3.2cm");
    }

    #[test]
    fn 快照摘要固定且随内容变化() {
        let a = HistorySnapshot::capture_at(vec![report("S-L", "kidney.left", 28.0)], 1_700_000_000);
        let b = HistorySnapshot::capture_at(vec![report("S-L", "kidney.left", 28.0)], 1_700_000_900);
        let c = HistorySnapshot::capture_at(vec![report("S-L", "kidney.left", 30.0)], 1_700_000_900);
        assert_eq!(a.digest(), b.digest());
        assert_ne!(a.digest(), c.digest());
        assert!(detect_drift(&a, &b).is_none());
        assert!(detect_drift(&a, &c).is_some());
    }
}
