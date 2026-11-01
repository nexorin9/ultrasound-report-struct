//! 对拍入账：**一份已落台账的报告 → 与 RIS 同侧历史值对拍 → 结果入 `comparisons` 表**。
//!
//! ## 这一层补的是哪条接缝
//!
//! 对拍内核（[`crate::compare::round`]）只负责「算出结论」；
//! 落库与留痕由 [`crate::state::repo`] 负责 —— 中间**缺的那一段**就是这一层：
//! 谁把「一份报告 + 一份历史」变成「台账里可核对的差值行」。
//!
//! 缺了它，差值就只活在命令行的一次打印里 —— 医师确认时读到的是**空差值**，
//! 回写给 EMR 的报文里也就没有 `comparisons`。
//!
//! ## 一轮对拍的完整动作
//!
//! ```text
//! 台账行（已落库） ──┐
//!                    ├─► 可对拍筛选 ─► RIS 取数 ─► 三态判定 ─► comparisons 行
//! RIS 历史（出站读） ─┘                    │
//!                                         └─ 够不着 ─► comparison_skipped（不产差值）
//! ```
//!
//! ## 三条硬承诺
//!
//! 1. **只读台账，不重新解析原文** —— 值、单位码、强转路径取台账里那一份
//!    （重新解析会得到另一套口径，差值就跟着漂）；
//! 2. **RIS 够不着 ≠ 没有历史** —— 落 `comparison_skipped` + 上游错误码，
//!    **不产差值**、不冒充首次（见 [`crate::compare::round`]）；
//! 3. **对完拍就进人工确认队列** —— 报告状态推进到 `awaiting_review`
//!    （草稿态，等医师确认；**不自动回写**）。
//!
//! ## 出站边界
//!
//! RIS 取数走**真实出站读**（[`crate::compare::ris_client::RisClient`]，
//! `GET {RIS_BASE_URL}/api/v1/ultrasound-reports`，Bearer 凭据只从环境变量读）。
//! 本地 HTTP 替身**只在测试数据层**，不是最终对接边界。

use crate::compare::ris_client::{RisClient, RisError, RisQuery};
use crate::compare::round::ComparisonRound;
use crate::compare::states::ComparisonVerdict;
use crate::compare::{select_comparable, ComparisonState};
use crate::parse::ParsedItem;
use crate::state::repo::{NewComparison, Repo, RepoError};
use crate::workflow::audit::{Actor, AuditContext, AuditRecorder};
use crate::workflow::state::{transition, Status};

/// 一次对拍的结果（**报告级**）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ComparisonOutcome {
    /// 报告级对拍状态：取本次各测值状态的**收口**口径。
    ///
    /// 收口规则（**从严**）：只要有一条测值没比成，本报告就算没比成 ——
    /// 报告级状态宁可保守，医师看到「没比成」再去查 RIS，
    /// 看到「比过了」却漏了一条，那是静默出错。
    pub state: String,
    /// 参与对拍的测值条数。
    pub compared: usize,
    /// 产出差值的条数。
    pub deltas: usize,
    /// 首次检查（无历史）的条数。
    pub first_time: usize,
    /// 多历史歧义的条数。
    pub ambiguous: usize,
    /// 本轮没比成的条数（RIS 够不着）。
    pub skipped: usize,
    /// 是否处于降级状态（RIS 够不着）。
    pub degraded: bool,
    /// 降级时的上游错误码（稳定码，运维按码聚合）。
    pub error_code: Option<String>,
    /// 对拍后报告的状态（`awaiting_review` = 已进人工确认队列）。
    pub report_status: String,
}

/// 对拍入账的错误。
#[derive(Debug)]
pub enum CompareError {
    /// 状态库错误。
    Repo(RepoError),
    /// 状态库里没有这份报告。
    ReportNotFound(String),
    /// 报告当前状态不允许进入对拍（例如已经是终态）。
    IllegalState(String),
}

impl std::fmt::Display for CompareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompareError::Repo(e) => write!(f, "状态库错误：{}", e),
            CompareError::ReportNotFound(study_no) => {
                write!(f, "状态库里没有检查号 {} 的报告", study_no)
            }
            CompareError::IllegalState(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for CompareError {}

impl From<RepoError> for CompareError {
    fn from(e: RepoError) -> Self {
        CompareError::Repo(e)
    }
}

/// **对拍入账**：把一份已落台账的报告与 RIS 同侧历史值对拍，结果落 `comparisons`。
///
/// - `client` 为 `None` → 视作 **RIS 端点或凭据缺失**（`RIS_NOT_CONFIGURED`），
///   本轮显式跳过，**不产差值**；
/// - `client` 取数失败（超时 / 5xx / 鉴权 / 报文不合契约）→ 同样显式跳过，
///   并把上游错误码留在结论与留痕里。
pub fn compare_report(
    repo: &Repo,
    study_no: &str,
    client: Option<&RisClient>,
) -> Result<ComparisonOutcome, RepoError> {
    compare_report_detailed(repo, study_no, client)
        .map_err(|e| match e {
            CompareError::Repo(e) => e,
            CompareError::ReportNotFound(_) => {
                RepoError::NotFound(format!("状态库里没有检查号 {} 的报告", study_no))
            }
            CompareError::IllegalState(msg) => RepoError::InvalidItem(msg),
        })
}

/// **对拍入账**（带更细的错误语义）。
///
/// 与 [`compare_report`] 同一实现 —— 这个函数保留细分错误类型，
/// 调用方（守护调度）要分得清「报告不在库里」与「状态不对」。
pub fn compare_report_detailed(
    repo: &Repo,
    study_no: &str,
    client: Option<&RisClient>,
) -> Result<ComparisonOutcome, CompareError> {
    let report = repo
        .find_by_study_no(study_no)?
        .ok_or_else(|| CompareError::ReportNotFound(study_no.to_string()))?;
    compare_report_row(repo, &report, client)
}

/// **对拍入账**（拿现成的报告行 —— 调用方已经查到了，不必再查一遍）。
///
/// HL7 通道的检查号取自 `OBR-3`，与投递目录名可能不同 ——
/// 守护调度那侧已经握着报告行，走这个入口不必拿检查号反查。
pub fn compare_report_row(
    repo: &Repo,
    report: &crate::state::repo::ReportRow,
    client: Option<&RisClient>,
) -> Result<ComparisonOutcome, CompareError> {
    let report = report.clone();

    // 终态报告不再对拍（已经回写或被驳回 —— 再对一次没有业务含义）。
    let current = Status::lookup(&report.status).ok_or_else(|| {
        CompareError::IllegalState(format!(
            "报告状态 {:?} 不在状态机取值集合里",
            report.status
        ))
    })?;
    if current.is_terminal() {
        return Err(CompareError::IllegalState(format!(
            "报告当前状态是 {}（{}），不再对拍",
            current.as_str(),
            current.message()
        )));
    }

    // 台账行 → 解析条目（**只做形状转换，不重新解析**）。
    let rows = repo.items_of_report(report.id)?;
    let items: Vec<ParsedItem> = rows.iter().map(|row| row.to_parsed_item()).collect();
    // 失败条目（报告级阻断码在这一步生效）。
    let failures = repo.failures_of_report(report.id)?;

    let mut outcome = ComparisonOutcome {
        state: ComparisonState::Skipped.as_str().to_string(),
        report_status: report.status.clone(),
        ..ComparisonOutcome::default()
    };

    // 可对拍的测值（按码表策略筛；报告级阻断则整份不进对拍）。
    let selection = select_comparable(&items, &failures);

    // **一轮对拍**：固定取数时点 + 固定历史快照。
    let (round, degradation) = match client {
        Some(client) => {
            let patient_id = report.patient_id.clone().unwrap_or_default();
            let query = RisQuery::for_patient(patient_id);
            match client.fetch_history(&query) {
                Ok(reports) => (ComparisonRound::from_reports(reports), None),
                Err(err) => (ComparisonRound::from_ris_error(&err), Some(err)),
            }
        }
        None => {
            let err = RisError::not_configured("RIS 端点或凭据未配置（出站读不可用）");
            (ComparisonRound::from_ris_error(&err), Some(err))
        }
    };
    outcome.degraded = degradation.is_some();
    if let Some(err) = &degradation {
        outcome.error_code = Some(err.code().to_string());
    }

    let patient_id = report.patient_id.clone().unwrap_or_default();
    let recorder = AuditRecorder::new(repo);
    let ctx = AuditContext::new(
        report.id,
        &report.study_no,
        &report.idempotency_key,
        &report.dictionary_version,
    );

    for item in selection.comparable {
        let verdict = match round.compare_item(&patient_id, item) {
            Some(verdict) => verdict,
            // 当前测值本身不可用 —— 不判成「首次」（那是「没比过」，不是「没有历史」）。
            None => continue,
        };
        outcome.compared += 1;
        store_verdict(repo, &recorder, &ctx, report.id, &rows, item, &verdict, &mut outcome)?;
    }

    // 报告级收口（**从严**）：
    //
    // | 情形 | 报告级状态 | 为什么 |
    // |---|---|---|
    // | 一条都没比成（`compared == 0`） | `comparison_skipped` | **没比过 ≠ 没有历史** —— 判成「首次」是撒谎 |
    // | 有测值没比成（RIS 够不着） | `comparison_skipped` | 同上，带上游错误码 |
    // | 命中多条历史 | `ambiguous` | 差值无法唯一确定，交人工 |
    // | 至少一条对上了 | `has_history` | 这一轮确实比成了 |
    // | 比过了但都没有历史 | `first_time` | **正常业务结果**，不是失败 |
    outcome.state = if outcome.compared == 0 || outcome.skipped > 0 {
        ComparisonState::Skipped.as_str().to_string()
    } else if outcome.ambiguous > 0 {
        ComparisonState::Ambiguous.as_str().to_string()
    } else if outcome.deltas > 0 {
        ComparisonState::HasHistory.as_str().to_string()
    } else {
        ComparisonState::FirstTime.as_str().to_string()
    };

    // **对拍留痕**：一轮一条，写明比了哪些、差值多少、状态是什么。
    let detail = format!(
        "对拍完成：参与 {} 条，产出差值 {} 条（首次 {} / 歧义 {} / 没比成 {}）{}",
        outcome.compared,
        outcome.deltas,
        outcome.first_time,
        outcome.ambiguous,
        outcome.skipped,
        match &outcome.error_code {
            Some(code) => format!("；上游错误码 {}", code),
            None => String::new(),
        }
    );
    recorder.compared(&ctx, &Actor::System, Some(&detail))?;

    // **进人工确认队列**：草稿态等人看（**不自动回写**）。
    if current == Status::Parsed {
        let next = transition(Status::Parsed, Status::AwaitingReview)
            .map_err(|e| CompareError::IllegalState(e.to_string()))?;
        repo.set_report_status(report.id, next.as_str())?;
        outcome.report_status = next.as_str().to_string();
    }

    Ok(outcome)
}

/// 落一条对拍结论（差值行 + 必要时的失败留痕）。
#[allow(clippy::too_many_arguments)]
fn store_verdict(
    repo: &Repo,
    recorder: &AuditRecorder,
    ctx: &AuditContext,
    report_id: i64,
    rows: &[crate::state::repo::ItemRow],
    item: &ParsedItem,
    verdict: &ComparisonVerdict,
    outcome: &mut ComparisonOutcome,
) -> Result<(), CompareError> {
    match verdict.state {
        ComparisonState::HasHistory => {
            let delta = verdict.delta.as_ref();
            repo.insert_comparison(&NewComparison {
                report_id,
                item_id: item_id_of(rows, item),
                part_key: item.part_key.clone(),
                item_code: item.item_code.clone(),
                previous_value: delta.map(|d| d.previous_value),
                current_value: delta.map(|d| d.current_value),
                delta: delta.map(|d| d.delta),
                direction: delta.map(|d| d.direction.as_str().to_string()),
                unit_code: delta.map(|d| d.unit_code.clone()),
                state: verdict.state.as_str().to_string(),
            })?;
            outcome.deltas += 1;
        }
        ComparisonState::FirstTime => {
            // 首次不是失败：不产差值，也不产失败条目。
            repo.insert_comparison(&NewComparison {
                report_id,
                item_id: item_id_of(rows, item),
                part_key: item.part_key.clone(),
                item_code: item.item_code.clone(),
                previous_value: None,
                current_value: item.value_si,
                delta: None,
                direction: None,
                unit_code: item.unit_code.clone(),
                state: verdict.state.as_str().to_string(),
            })?;
            outcome.first_time += 1;
        }
        ComparisonState::Ambiguous => {
            // 歧义：**不产差值**（静默取一条会给出看起来正常的错差值）。
            repo.insert_comparison(&NewComparison {
                report_id,
                item_id: item_id_of(rows, item),
                part_key: item.part_key.clone(),
                item_code: item.item_code.clone(),
                previous_value: None,
                current_value: item.value_si,
                delta: None,
                direction: None,
                unit_code: item.unit_code.clone(),
                state: verdict.state.as_str().to_string(),
            })?;
            outcome.ambiguous += 1;
            if let Some(failure) = &verdict.failure {
                recorder.failed(ctx, &Actor::System, failure)?;
            }
        }
        ComparisonState::Skipped => {
            // 没比成：不产差值，带上游错误码留痕。
            repo.insert_comparison(&NewComparison {
                report_id,
                item_id: item_id_of(rows, item),
                part_key: item.part_key.clone(),
                item_code: item.item_code.clone(),
                previous_value: None,
                current_value: item.value_si,
                delta: None,
                direction: None,
                unit_code: item.unit_code.clone(),
                state: verdict.state.as_str().to_string(),
            })?;
            outcome.skipped += 1;
            if let Some(failure) = &verdict.failure {
                recorder.failed(ctx, &Actor::System, failure)?;
            }
        }
    }
    Ok(())
}

/// 台账行 → 主键（按部位键 + 测项码 + 位置对上号）。
///
/// 对不上就留空（**不猜**）：差值行的 `item_id` 只是关联指针，
/// 猜错会把差值挂到别的测值上 —— 比没有指针更糟。
fn item_id_of(
    rows: &[crate::state::repo::ItemRow],
    item: &ParsedItem,
) -> Option<i64> {
    rows.iter()
        .find(|row| {
            row.part_key == item.part_key
                && row.item_code == item.item_code
                && row.line == item.line
                && row.column == item.column
        })
        .map(|row| row.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_report;
    use crate::state::idempotency_key;
    use crate::state::repo::IngestRecord;

    /// 一份**键齐全**的脱敏报告（部位 + 测项 + 数值 + 单位）。
    const REPORT_OK: &str =
        "病历号：PID-EXAMPLE\n超声所见\n左肾前后径 3.2cm\n提示：左肾积水，建议结合临床复查。";

    /// 一份**测项键不齐**的报告（「积水」不在测项词典里 → 键不完整）。
    const REPORT_NO_ITEM_CODE: &str =
        "病历号：PID-EXAMPLE\n超声所见\n左肾积水 3.2cm\n提示：左肾积水，建议结合临床复查。";

    /// 落一份报告（解析 → 强转 → 落台账）。
    fn seed(repo: &Repo, study_no: &str, body: &str) {
        let outcome = parse_report(body);
        repo.commit_ingestion(&IngestRecord {
            study_no,
            patient_id: Some("PID-EXAMPLE"),
            source_path: None,
            idempotency_key: &idempotency_key(study_no, body),
            dictionary_version: crate::dictionary_version(),
            items: &outcome.items,
            failures: &outcome.failures,
        })
        .expect("报告应能落库");
    }

    /// 差值行数。
    fn comparison_rows(repo: &Repo, report_id: i64) -> i64 {
        repo.connection()
            .query_row(
                "SELECT COUNT(*) FROM comparisons WHERE report_id = ?1",
                rusqlite::params![report_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn 没有_ris_客户端时显式降级且不产差值() {
        let repo = Repo::open_in_memory().unwrap();
        seed(&repo, "S-PIPE-1", REPORT_OK);
        let outcome = compare_report(&repo, "S-PIPE-1", None).unwrap();
        assert!(outcome.degraded, "没配 RIS 就该降级");
        assert_eq!(outcome.state, "comparison_skipped");
        assert_eq!(outcome.deltas, 0, "没比成就不该有差值");
        assert_eq!(outcome.skipped, 1);
        assert_eq!(
            outcome.error_code.as_deref(),
            Some("RIS_NOT_CONFIGURED"),
            "降级要带上游错误码"
        );
        // 台账行不丢，报告照常进人工确认队列。
        let report = repo.find_by_study_no("S-PIPE-1").unwrap().unwrap();
        assert_eq!(comparison_rows(&repo, report.id), 1);
        assert_eq!(outcome.report_status, "awaiting_review");
    }

    #[test]
    fn 一条都没比成时不冒充首次() {
        let repo = Repo::open_in_memory().unwrap();
        seed(&repo, "S-PIPE-2", REPORT_NO_ITEM_CODE);
        let outcome = compare_report(&repo, "S-PIPE-2", None).unwrap();
        assert_eq!(outcome.compared, 0, "键不齐的测值进不了对拍");
        assert_eq!(
            outcome.state, "comparison_skipped",
            "没比过 ≠ 没有历史：不能判成首次"
        );
    }

    #[test]
    fn 终态报告拒绝对拍() {
        let repo = Repo::open_in_memory().unwrap();
        seed(&repo, "S-PIPE-3", REPORT_OK);
        let report = repo.find_by_study_no("S-PIPE-3").unwrap().unwrap();
        repo.set_report_status(report.id, "written_back").unwrap();
        let err = compare_report_detailed(&repo, "S-PIPE-3", None).unwrap_err();
        match err {
            CompareError::IllegalState(msg) => {
                assert!(msg.contains("written_back"), "拒绝理由要说得清：{}", msg)
            }
            other => panic!("应是非法状态拒绝，实际：{}", other),
        }
    }

    #[test]
    fn 库里没有这份报告时明确报错() {
        let repo = Repo::open_in_memory().unwrap();
        let err = compare_report_detailed(&repo, "S-NOT-THERE", None).unwrap_err();
        assert!(err.to_string().contains("S-NOT-THERE"), "{}", err);
    }

    #[test]
    fn 对拍只读台账_不重新解析原文() {
        let repo = Repo::open_in_memory().unwrap();
        seed(&repo, "S-PIPE-4", REPORT_OK);
        // 台账行被改成别的值（模拟「台账与原文口径不一致」）——
        // 对拍用的是**台账里那一份**，不是重新解析原文。
        repo.connection()
            .execute(
                "UPDATE items SET value_si = 40.0, unit_code = 'mm' WHERE report_id = \
                 (SELECT id FROM reports WHERE study_no = 'S-PIPE-4')",
                [],
            )
            .unwrap();
        let report = repo.find_by_study_no("S-PIPE-4").unwrap().unwrap();
        let items = repo.items_of_report(report.id).unwrap();
        let parsed = items[0].to_parsed_item();
        assert_eq!(parsed.value_si, Some(40.0), "形状转换取台账的值");
        assert_eq!(parsed.unit_code.as_deref(), Some("mm"));
        assert_eq!(parsed.part_key.as_deref(), Some("kidney.left"));
    }
}
