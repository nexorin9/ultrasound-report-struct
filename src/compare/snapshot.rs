//! 对拍引擎第 4 层（二）：**历史快照一致性**。
//!
//! **这一层解决的接缝**：一份报告里有三条测值，如果每条都单独回 RIS 取一次历史，
//! 三条测值的「参照基线」就可能来自**三个不同时刻** ——
//! 上一条还在时，下一条已经变了，差值算出来彼此不可比。
//! 报告一签发，医师看到三条差值，**却不知道它们不是同一把尺子量的**。
//!
//! **口径**：**一轮对拍 = 一个固定取数时点**。
//! 一次取数的结果就是一份**快照**（[`HistorySnapshot`]）：
//! 取数时点 + 历史内容 + 内容摘要（sha256）。
//! 这一轮里所有测值**共用同一份快照** —— 尺子只有一把。
//!
//! **快照一致性检查**：取数之后若再取一次（或 RIS 侧数据在期间被改动），
//! 两份快照的摘要会不一样 —— 这时**标记快照变化并留痕**
//! （[`SnapshotDrift`]），而不是装作没发生。
//!
//! **漂移了要不要改差值**：**不改**。
//! 差值是对**固定快照**算出来的 —— 那是对拍的可核对依据，
//! 拿一份会变的基线去重算，结果反而不可复现。
//! 漂移是**留痕**：让运维知道「RIS 侧在这期间动过」，
//! 需不需要重取由人决定（本产品不替人决定）。
//!
//! **摘要算法**：sha256 over **规范化序列化** ——
//! 报告按检查号排序（RIS 返回顺序变了不算历史变了），
//! 测量值**保持 RIS 原顺序**（同一测项出现两条时，顺序决定哪条在前，
//! 顺序变了就是要看得见的改动）。
//!
//! **与失败码表的关系**：快照漂移**不是**失败 ——
//! 测值是对的、对拍也是做成了的，漂移只是「参照基线在期间动过」这一事实。
//! 所以它走**留痕码** [`SnapshotDrift::CHANGE_CODE`]（进审计），
//! 不进失败清单（那是「认不出的值」的清单），也**不新增**错误码 ——
//! 错误码表是运维按码聚合的唯一真源，不为留痕而扩表。

use sha2::{Digest, Sha256};

use crate::compare::ris_client::RisReport;
use crate::state::{datetime_from_unix, utc_now_secs};

/// **历史快照**：一次取数的结果 + 取数时点 + 内容摘要。
///
/// 快照是**不可变**的 —— 拿到就定住，这一轮对拍期间不再变。
#[derive(Clone, Debug, PartialEq)]
pub struct HistorySnapshot {
    /// 取回的历史报告（按取数原样保存）。
    reports: Vec<RisReport>,
    /// 内容摘要（sha256 十六进制）。
    digest: String,
    /// 取数时点（`YYYY-MM-DD HH:mm:ss`，UTC）。
    taken_at: String,
    /// 取数时点（Unix 秒）。
    taken_at_unix: u64,
}

impl HistorySnapshot {
    /// 捕获一份快照（取数时点 = 现在）。
    pub fn capture(reports: Vec<RisReport>) -> HistorySnapshot {
        HistorySnapshot::capture_at(reports, utc_now_secs())
    }

    /// 捕获一份快照（**指定**取数时点，便于复现与回归）。
    pub fn capture_at(reports: Vec<RisReport>, unix_secs: u64) -> HistorySnapshot {
        let digest = digest_of(&reports);
        HistorySnapshot {
            reports,
            digest,
            taken_at: datetime_from_unix(unix_secs),
            taken_at_unix: unix_secs,
        }
    }

    /// 快照里的历史报告。
    pub fn reports(&self) -> &[RisReport] {
        &self.reports
    }

    /// 内容摘要（sha256 十六进制）。
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// 取数时点（`YYYY-MM-DD HH:mm:ss`）。
    pub fn taken_at(&self) -> &str {
        &self.taken_at
    }

    /// 取数时点（Unix 秒）。
    pub fn taken_at_unix(&self) -> u64 {
        self.taken_at_unix
    }

    /// 快照是否为空（**没有历史**）。
    pub fn is_empty(&self) -> bool {
        self.reports.is_empty()
    }

    /// 快照里的历史报告条数。
    pub fn len(&self) -> usize {
        self.reports.len()
    }
}

/// **快照漂移**：两次取数之间，历史内容变了。
///
/// 逐条列出**哪几份**历史变了 —— 运维据此判断是「新增了一份历史」
/// 还是「某份历史被改了」，两种情况的处置动作不一样。
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotDrift {
    /// 留痕码（**不是**失败错误码 —— 失败码表不因留痕而扩表）。
    pub change_code: &'static str,
    /// 变化前摘要。
    pub base_digest: String,
    /// 变化后摘要。
    pub later_digest: String,
    /// **内容变了**的历史检查号（同一份历史，数值 / 单位 / 测项被改动）。
    pub changed_studies: Vec<String>,
    /// **新增**的历史检查号。
    pub added_studies: Vec<String>,
    /// **消失**的历史检查号。
    pub removed_studies: Vec<String>,
}

impl SnapshotDrift {
    /// **留痕码**：快照变化。
    ///
    /// 与失败清单的错误码是**两套东西**：这里记的是「参照基线动过」这一事实，
    /// 不是「这条测值认不出来」。审计与运行摘要按它聚合。
    pub const CHANGE_CODE: &'static str = "SNAPSHOT_CHANGED";

    /// 是否真的变了（三类变化一个都没有就是没变）。
    pub fn is_empty(&self) -> bool {
        self.changed_studies.is_empty() && self.added_studies.is_empty() && self.removed_studies.is_empty()
    }

    /// 面向人的说明（审计详情直接展示）。
    pub fn message(&self) -> String {
        format!(
            "取数期间历史发生变化：内容变更 {} 份、新增 {} 份、消失 {} 份（差值仍按固定快照计算，需人工确认是否重取）",
            self.changed_studies.len(),
            self.added_studies.len(),
            self.removed_studies.len()
        )
    }
}

/// **检测快照漂移**：两份快照不一致时给出变化明细，一致时返回 `None`。
///
/// **逐份比对**而不是只看摘要 —— 摘要只回答「变没变」，
/// 逐份比对才回答「**哪几份**变了」，那才是运维要动手的地方。
pub fn detect_drift(base: &HistorySnapshot, later: &HistorySnapshot) -> Option<SnapshotDrift> {
    if base.digest() == later.digest() {
        return None;
    }

    let mut changed = Vec::new();
    let mut added = Vec::new();
    let mut removed = Vec::new();

    for later_report in later.reports() {
        match base.reports().iter().find(|r| r.study_no == later_report.study_no) {
            Some(base_report) => {
                if canonical_of_report(base_report) != canonical_of_report(later_report) {
                    changed.push(later_report.study_no.clone());
                }
            }
            None => added.push(later_report.study_no.clone()),
        }
    }
    for base_report in base.reports() {
        if !later.reports().iter().any(|r| r.study_no == base_report.study_no) {
            removed.push(base_report.study_no.clone());
        }
    }

    changed.sort();
    added.sort();
    removed.sort();

    Some(SnapshotDrift {
        change_code: SnapshotDrift::CHANGE_CODE,
        base_digest: base.digest().to_string(),
        later_digest: later.digest().to_string(),
        changed_studies: changed,
        added_studies: added,
        removed_studies: removed,
    })
}

/// 内容摘要（sha256 十六进制）。
///
/// **规范化序列化**：报告按检查号排序（RIS 返回顺序变了不算历史变了），
/// 测量值保持 RIS 原顺序（同一测项两条时顺序有含义）。
pub fn digest_of(reports: &[RisReport]) -> String {
    let mut sorted: Vec<&RisReport> = reports.iter().collect();
    sorted.sort_by(|a, b| a.study_no.cmp(&b.study_no));
    let mut canonical = String::new();
    for report in sorted {
        canonical.push_str(&canonical_of_report(report));
        canonical.push('\n');
    }
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

/// 一份历史报告的规范化文本（进摘要用）。
fn canonical_of_report(report: &RisReport) -> String {
    let mut text = String::new();
    text.push_str("study_no=");
    text.push_str(&report.study_no);
    text.push_str(";patient_id=");
    text.push_str(report.patient_id.as_deref().unwrap_or(""));
    text.push_str(";body_part=");
    text.push_str(report.body_part.as_deref().unwrap_or(""));
    text.push_str(";performed_at=");
    text.push_str(report.performed_at.as_deref().unwrap_or(""));
    text.push_str(";measurements=");
    for measurement in &report.measurements {
        text.push_str(measurement.item_code.as_str());
        text.push('=');
        match measurement.value {
            Some(value) => text.push_str(&format!("{}", value)),
            None => text.push_str(""),
        }
        text.push('/');
        text.push_str(measurement.unit.as_deref().unwrap_or(""));
        text.push(',');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::ris_client::RisReportMeasurement;

    fn report(study_no: &str, value: f64, unit: &str) -> RisReport {
        RisReport {
            study_no: study_no.to_string(),
            patient_id: Some("PID-EXAMPLE".to_string()),
            body_part: Some("kidney.left".to_string()),
            performed_at: Some("2026-01-01".to_string()),
            measurements: vec![RisReportMeasurement {
                item_code: "kidney.ap_diameter".to_string(),
                value: Some(value),
                unit: Some(unit.to_string()),
            }],
        }
    }

    #[test]
    fn 同一份内容两次取数摘要一致() {
        let a = HistorySnapshot::capture_at(vec![report("S-1", 28.0, "mm")], 1_700_000_000);
        let b = HistorySnapshot::capture_at(vec![report("S-1", 28.0, "mm")], 1_700_000_600);
        assert_eq!(a.digest(), b.digest());
        assert!(detect_drift(&a, &b).is_none());
    }

    #[test]
    fn 取数时点固定并可查() {
        let snap = HistorySnapshot::capture_at(vec![report("S-1", 28.0, "mm")], 1_700_000_000);
        assert_eq!(snap.taken_at(), "2023-11-14 22:13:20");
        assert_eq!(snap.taken_at_unix(), 1_700_000_000);
        assert_eq!(snap.len(), 1);
        assert!(!snap.is_empty());
    }

    #[test]
    fn 数值变了算内容变更() {
        let base = HistorySnapshot::capture_at(vec![report("S-1", 28.0, "mm")], 1_700_000_000);
        let later = HistorySnapshot::capture_at(vec![report("S-1", 30.0, "mm")], 1_700_000_600);
        let drift = detect_drift(&base, &later).expect("应检出漂移");
        assert_eq!(drift.change_code, "SNAPSHOT_CHANGED");
        assert_eq!(drift.changed_studies, vec!["S-1".to_string()]);
        assert!(drift.added_studies.is_empty());
        assert!(drift.removed_studies.is_empty());
        assert!(!drift.is_empty());
        assert!(drift.message().contains("内容变更 1 份"));
    }

    #[test]
    fn 新增与消失的历史逐条列出() {
        let base = HistorySnapshot::capture_at(
            vec![report("S-1", 28.0, "mm"), report("S-2", 25.0, "mm")],
            1_700_000_000,
        );
        let later = HistorySnapshot::capture_at(
            vec![report("S-1", 28.0, "mm"), report("S-3", 31.0, "mm")],
            1_700_000_600,
        );
        let drift = detect_drift(&base, &later).expect("应检出漂移");
        assert!(drift.changed_studies.is_empty());
        assert_eq!(drift.added_studies, vec!["S-3".to_string()]);
        assert_eq!(drift.removed_studies, vec!["S-2".to_string()]);
    }

    #[test]
    fn 返回顺序变了不算历史变了() {
        // RIS 把同一批历史换个顺序返回 —— 内容没变，不该报漂移。
        let base = HistorySnapshot::capture_at(
            vec![report("S-1", 28.0, "mm"), report("S-2", 25.0, "mm")],
            1_700_000_000,
        );
        let later = HistorySnapshot::capture_at(
            vec![report("S-2", 25.0, "mm"), report("S-1", 28.0, "mm")],
            1_700_000_600,
        );
        assert!(detect_drift(&base, &later).is_none());
    }

    #[test]
    fn 空快照与空快照不漂移() {
        let a = HistorySnapshot::capture_at(Vec::new(), 1_700_000_000);
        let b = HistorySnapshot::capture_at(Vec::new(), 1_700_000_600);
        assert!(a.is_empty());
        assert!(detect_drift(&a, &b).is_none());
    }

    #[test]
    fn 摘要稳定且与内容绑定() {
        let a = digest_of(&[report("S-1", 28.0, "mm")]);
        let b = digest_of(&[report("S-1", 28.0, "mm")]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(a, digest_of(&[report("S-1", 29.0, "mm")]));
        assert_ne!(a, digest_of(&[report("S-2", 28.0, "mm")]));
    }
}
