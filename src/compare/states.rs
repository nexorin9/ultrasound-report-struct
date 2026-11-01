//! 对拍引擎第 3 层：三态判定（有历史 / 无历史 / 多历史歧义）。
//!
//! **这一层解决的接缝**：超声医师签报告前要回答一句话 ——
//! 「这个患者**同侧**这个测项，跟上次比是大了还是小了」。
//! 上一层（[`crate::compare::r#match`]）已经能把差值算出来，
//! 但**算得出来不等于能给人看** —— 得先说清楚这轮对拍**到底算没算成**。
//!
//! **三态**（与状态库 `comparisons.state` 的取值一一对应）：
//!
//! | 状态 | 含义 | 差值 | 是否失败 |
//! |---|---|---|---|
//! | `has_history` | **有历史**：恰好命中一条 | 有 | 否 |
//! | `first_time` | **无历史（首次）**：一条都没命中 | 无 | 否（正常结果） |
//! | `ambiguous` | **多历史歧义**：命中多条 | **无** | 是（待人工判定） |
//!
//! **为什么「首次」不是失败**：患者第一次做这项检查，本来就没有上次 ——
//! 这是**正常的业务事实**，不是错误。把它算成失败，
//! 失败清单里就会塞满「首次检查」，真正要人处理的事反而被淹掉。
//!
//! **歧义为什么不任取其一**：同一规范键命中多条历史时，
//! 任取一条可能**指错对象**（拿另一份报告的值当参照）——
//! 差值算得出来、看着正常，结论却可能指错。
//! 这是差值类台账**最危险**的失败方式：它不报错，它只是悄悄给错答案。
//! 所以本层**一律不产差值**，产出 `COMPARISON_AMBIGUOUS` 失败条目，
//! 把候选历史逐条列出来交给人判（医师指定参照哪一份）。
//!
//! **失败码不另造**：歧义走**全项目共用的**稳定码表
//! （[`crate::parse::errors`]）—— 运维按错误码聚合时，
//! 「同键命中多条历史」与「测值缺单位」在同一张表里，不出现第二套口径。
//!
//! **立场溯源（改写，不复制源代码）**：源产品的强转接口在**查不到合法路径**时
//! 返回显式的「无路径」，而不是猜一个单位继续算（见
//! `github_ref/pgrust/crates/backend/parser/coerce_seams/src/lib.rs`
//! 的 `CoercionPathType` 与 `find_coercion_pathway_implicit`）。
//! 本层是**同一立场的另一处落点**：**无法唯一判定就显式失败，不猜一个答案**。
//! 源产品没有「多候选历史」这个概念（它是数据库侧的产品），
//! 这里的映射是**立场同构**，不是代码移植。
//!
//! **本层不做的两件事**（留给后续任务，边界写在这里免得越界）：
//!
//! - **同侧可比性规则**（双侧 vs 单侧是否可比）由 Task 31 的侧别规则层落地
//!   （见 [`crate::compare::side`]）；本层的部位口径是**规范部位键精确相等**
//!   （上一层的口径，匹配层已按侧别规则表细化）。
//! - **上游不可达降级**（`comparison_skipped: upstream_unavailable`）由 Task 31 落地
//!   （见 [`crate::compare::round`]）；本层只处理「历史**取到了**之后」的三态。
//!
//! **Task 31 追加的第四个状态**：`comparison_skipped`（本层新增）——
//! RIS 够不着时**这一轮没比成**。它**不是**「首次检查」：
//! 首次是正常业务结果（患者第一次做这项），跳过是系统没做成。
//! 混成一样，医师会以为「跟上次一样」，而其实压根没比成。

use std::collections::BTreeSet;

use crate::compare::r#match::{
    match_history, ComparisonKey, CurrentMeasure, Delta, HistoryHit, MatchOutcome, SkipReason,
};
use crate::compare::ris_client::RisReport;
use crate::compare::snapshot::SnapshotDrift;
use crate::parse::errors::{DetailCode, ErrorCode, Failure};
use crate::parse::extract::ParsedItem;

/// **对拍状态**（与状态库 `comparisons.state` 的取值一一对应）。
///
/// 状态是**落库字段**，取值必须稳定 —— 状态变了等于历史记录作废。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ComparisonState {
    /// `has_history` —— 有历史：恰好命中一条，差值已产出。
    HasHistory,
    /// `first_time` —— 无历史（首次）：一条历史都没命中，**不是失败**，是正常结果。
    FirstTime,
    /// `ambiguous` —— 多历史歧义：命中多条，**绝不任取其一**。
    Ambiguous,
    /// `comparison_skipped` —— **本轮没比成**：RIS 够不着（上游不可达 / 鉴权失败 /
    /// 响应不合契约 / 未配置）。
    ///
    /// **绝不是「无差异」**：这一轮压根没取到历史值，
    /// 「跟上次一样」这句话在这份报告上**没有依据**。
    /// 与 `first_time` 严格分开 —— 后者是正常业务结果，前者是系统没做成。
    Skipped,
}

impl ComparisonState {
    /// **全部状态**（穷举清单）。
    pub const ALL: [ComparisonState; 4] = [
        ComparisonState::HasHistory,
        ComparisonState::FirstTime,
        ComparisonState::Ambiguous,
        ComparisonState::Skipped,
    ];

    /// 状态字符串（**落库字段**，与 `comparisons.state` 一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            ComparisonState::HasHistory => "has_history",
            ComparisonState::FirstTime => "first_time",
            ComparisonState::Ambiguous => "ambiguous",
            ComparisonState::Skipped => "comparison_skipped",
        }
    }

    /// 状态 ↔ 字符串往返（未知字符串返回 `None`）。
    ///
    /// 从库里读回状态时用它 —— 读到表里没有的状态就是**数据不对**，
    /// 宁可返回 `None` 让调用方显式处理，也不硬套一个默认状态。
    pub fn lookup(raw: &str) -> Option<ComparisonState> {
        ComparisonState::ALL
            .iter()
            .copied()
            .find(|state| state.as_str() == raw)
    }

    /// 中文说明（差值清单 / 企微推送直接展示）。
    pub fn message(self) -> &'static str {
        match self {
            ComparisonState::HasHistory => "有历史，已算出差值",
            ComparisonState::FirstTime => "无历史（首次检查），无差值",
            ComparisonState::Ambiguous => "多历史歧义，差值无法唯一确定，待人工判定",
            ComparisonState::Skipped => "RIS 历史取不到，本轮未对拍（不等于无差异）",
        }
    }
}

/// **当前测值在报告里的锚点**（歧义失败条目靠它指回原文）。
///
/// 失败条目的位置与片段是「照着这一段能在原报告里找到它」的锚点。
/// 对拍层的失败（歧义）指向的是**当前测值** ——
///
/// 「到底是哪一条测值跟历史对不上」是医师第一个要回答的问题。
///
/// 不知道位置时传 [`MeasureAnchor::unknown`]（位置 0、词面空）——
/// **位置未知就是 0，不编造**。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MeasureAnchor {
    /// 1 基行号（未知 0）。
    pub line: usize,
    /// 1 基列号（未知 0）。
    pub column: usize,
    /// 1 基字符光标（未知 0）。
    pub char_start: usize,
    /// 原样词面（值 + 单位，如 `3.2cm`）—— 取报告里的**原值原单位**。
    pub raw_text: String,
}

impl MeasureAnchor {
    /// 位置未知、词面为空（**不编造位置**）。
    pub fn unknown() -> MeasureAnchor {
        MeasureAnchor::default()
    }

    /// 从一条已解析条目取锚点（**原值原单位**，不是归一值）。
    pub fn from_item(item: &ParsedItem) -> MeasureAnchor {
        MeasureAnchor {
            line: item.line,
            column: item.column,
            char_start: item.char_start,
            raw_text: raw_measurement_text(item),
        }
    }
}

/// 当前测值的原样词面（值 + 单位），用作失败条目的原文锚点。
///
/// 取的是**报告里的原值原单位**（`3.2cm`），不是归一值（`32mm`）——
/// 医师在报告上找的是前者；归一值是本产品算出来的口径。
fn raw_measurement_text(item: &ParsedItem) -> String {
    let value = item.value_raw.map(|v| format!("{}", v)).unwrap_or_default();
    let unit = item
        .unit_raw
        .as_deref()
        .map(|u| u.trim())
        .filter(|u| !u.is_empty())
        .unwrap_or("");
    if value.is_empty() {
        String::new()
    } else {
        format!("{}{}", value, unit)
    }
}

/// **对拍判定的结论**（结构化结果）。
///
/// 四态各自带的东西不一样，一眼看得出这轮对拍算没算成：
///
/// - `has_history` —— 带 `delta`；
/// - `first_time` —— 不带 `delta`、**不带失败**（正常结果）；
/// - `ambiguous` —— **不带 `delta`**，带 `candidates`（全部候选）与 `failure`（失败条目）；
/// - `comparison_skipped` —— **不带 `delta`**，带 `failure`（上游错误码）。
#[derive(Clone, Debug, PartialEq)]
pub struct ComparisonVerdict {
    /// 本次对拍的规范键（患者 × 部位 × 测项）。
    pub key: ComparisonKey,
    /// 对拍状态。
    pub state: ComparisonState,
    /// 差值（**仅** `has_history` 有值）。
    pub delta: Option<Delta>,
    /// 命中条数（`0` / `1` / `>1`）。
    pub hit_count: usize,
    /// 歧义候选（**仅** `ambiguous` 有值）：同键的多条历史，逐条带检查号与归一值 ——
    /// 人工据此指定参照哪一份。
    pub candidates: Vec<HistoryHit>,
    /// 歧义失败条目（`ambiguous` / `comparison_skipped` 有值）：
    /// 稳定错误码 + 明细归类 + 原文位置。
    pub failure: Option<Failure>,
    /// 当前测值在报告里的锚点（**位置未知就是 0，不编造**）。
    ///
    /// 降级（`comparison_skipped`）时靠它告诉调用方「是哪一条测值没比成」——
    /// 上游错误本身不在报告正文里（位置 0），但**没比成的那条测值**在正文里有位置。
    pub anchor: MeasureAnchor,
    /// 快照漂移（取数期间历史变过 —— **留痕，不改差值**）。
    pub snapshot_drift: Option<SnapshotDrift>,
}

impl ComparisonVerdict {
    /// 是否有差值（**只有** `has_history` 有）。
    pub fn has_delta(&self) -> bool {
        self.delta.is_some()
    }

    /// 是否为多历史歧义。
    pub fn is_ambiguous(&self) -> bool {
        self.state == ComparisonState::Ambiguous
    }

    /// 是否为无历史（首次）。
    pub fn is_first_time(&self) -> bool {
        self.state == ComparisonState::FirstTime
    }

    /// 是否为**本轮没比成**（RIS 够不着）。
    pub fn is_skipped(&self) -> bool {
        self.state == ComparisonState::Skipped
    }

    /// **降级结论**：RIS 够不着 —— 不产差值，带上游错误码。
    ///
    /// **唯一**构造降级结论的入口（产品里「RIS 够不着」只有这一条路径）。
    pub fn skipped(key: ComparisonKey, failure: Failure) -> ComparisonVerdict {
        ComparisonVerdict {
            key,
            state: ComparisonState::Skipped,
            delta: None,
            hit_count: 0,
            candidates: Vec::new(),
            failure: Some(failure),
            anchor: MeasureAnchor::unknown(),
            snapshot_drift: None,
        }
    }

    /// 判定口径自洽（**唯一命中才有差值；歧义与跳过必带失败条目且不产差值**）。
    ///
    /// 这条不变量是本层的核心承诺，写成一个可复用的判据 ——
    /// 落库前、调差值清单前都可以拿它过一遍，
    /// 免得哪天有人图省事直接拿 `hits[0]` 算差值，把歧义又放过去了。
    pub fn is_consistent(&self) -> bool {
        match self.state {
            ComparisonState::HasHistory => {
                self.hit_count == 1 && self.delta.is_some() && self.failure.is_none()
            }
            ComparisonState::FirstTime => {
                self.hit_count == 0 && self.delta.is_none() && self.failure.is_none()
            }
            ComparisonState::Ambiguous => {
                self.hit_count > 1 && self.delta.is_none() && self.failure.is_some()
            }
            ComparisonState::Skipped => {
                // 没比成：**零命中、无差值、带上游失败条目**。
                // 绝不允许出现「跳过却带着差值」或「跳过却像首次一样干净」。
                self.hit_count == 0 && self.delta.is_none() && self.failure.is_some()
            }
        }
    }
}

/// **三态判定**（核心：从匹配结果判定）。
///
/// `anchor` 是当前测值在报告里的锚点 —— 歧义失败条目靠它指回原文；
/// 不知道位置时传 [`MeasureAnchor::unknown`]。
///
/// **返回 `None` 的唯一情形**：**当前测值本身不可用**
/// （`match_history` 已记下 `CURRENT_VALUE_MISSING`）。
/// 这时**不判成「首次」** —— 「没比过」不等于「没有历史」，
/// 混进三态里冒充首次，会让首次检查的统计口径整个错掉。
/// 当前测值不可用是**失败条目的事**（解析层已产出失败码），不是三态之一。
pub fn judge_history(outcome: &MatchOutcome, anchor: &MeasureAnchor) -> Option<ComparisonVerdict> {
    if outcome
        .skipped
        .iter()
        .any(|s| s.reason == SkipReason::CurrentValueMissing)
    {
        return None;
    }

    match outcome.hits.len() {
        // 恰好一条 —— 有历史，差值已给出。
        1 => Some(ComparisonVerdict {
            key: outcome.key.clone(),
            state: ComparisonState::HasHistory,
            delta: outcome.delta.clone(),
            hit_count: 1,
            candidates: Vec::new(),
            failure: None,
            anchor: anchor.clone(),
            snapshot_drift: None,
        }),
        // 一条都没有 —— 无历史（首次）。**正常结果，不是失败。**
        0 => Some(ComparisonVerdict {
            key: outcome.key.clone(),
            state: ComparisonState::FirstTime,
            delta: None,
            hit_count: 0,
            candidates: Vec::new(),
            failure: None,
            anchor: anchor.clone(),
            snapshot_drift: None,
        }),
        // 多条 —— 歧义。**绝不任取其一，不产差值**，交人工判定。
        n => {
            let candidates = outcome.hits.clone();
            let failure = ambiguity_failure(&candidates, anchor);
            Some(ComparisonVerdict {
                key: outcome.key.clone(),
                state: ComparisonState::Ambiguous,
                delta: None,
                hit_count: n,
                candidates,
                failure: Some(failure),
                anchor: anchor.clone(),
                snapshot_drift: None,
            })
        }
    }
}

/// **匹配 + 三态判定**（一步到位）。
///
/// 返回 `None` 的含义见 [`judge_history`]（当前测值不可用）。
pub fn judge(
    key: &ComparisonKey,
    current: &CurrentMeasure,
    history: &[RisReport],
) -> Option<ComparisonVerdict> {
    let outcome = match_history(key, current, history);
    judge_history(&outcome, &MeasureAnchor::unknown())
}

/// **从一条已解析条目发起三态判定**（规范键与原文锚点都从条目取）。
///
/// 这是**产品主路径上的调用方式**：解析层产出条目 → 本层对拍。
/// 部位键或测项码缺失 → `None`（键不全就没有可比的对象）。
pub fn judge_item(
    patient_id: &str,
    item: &ParsedItem,
    history: &[RisReport],
) -> Option<ComparisonVerdict> {
    let key = ComparisonKey::from_item(patient_id, item)?;
    let current = CurrentMeasure::from_item(item);
    let outcome = match_history(&key, &current, history);
    judge_history(&outcome, &MeasureAnchor::from_item(item))
}

/// 歧义失败条目：稳定码 + 明细归类 + 原文位置。
///
/// **明细两类**（运维要分得清是哪一种歧义）：
///
/// - 多份**历史报告**各命中一条 → `COMPARISON_MULTIPLE_REPORTS`；
/// - **同一份**历史报告内同一测项有多条 → `COMPARISON_MULTIPLE_MEASUREMENTS`。
///
/// **片段位**：优先放当前测值的原样词面（`3.2cm`）——
/// 医师照着它能在报告里找到那一条；
/// 没有词面时（位置未知）退而放**候选检查号**，
/// 说明「参照历史是哪几份」—— 同样是原文，不是说明文字。
fn ambiguity_failure(candidates: &[HistoryHit], anchor: &MeasureAnchor) -> Failure {
    let distinct_studies: BTreeSet<&str> = candidates.iter().map(|h| h.study_no.as_str()).collect();
    let detail = if distinct_studies.len() > 1 {
        DetailCode::ComparisonMultipleReports
    } else {
        DetailCode::ComparisonMultipleMeasurements
    };

    let snippet = if anchor.raw_text.trim().is_empty() {
        candidates
            .iter()
            .map(|h| h.study_no.as_str())
            .collect::<Vec<_>>()
            .join("、")
    } else {
        anchor.raw_text.clone()
    };

    Failure::new(
        ErrorCode::ComparisonAmbiguous,
        anchor.char_start,
        anchor.line,
        anchor.column,
        &snippet,
    )
    .with_detail_code(detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::ris_client::RisReportMeasurement;

    /// 造一份历史报告（三道闸都过的最小形状）。
    fn report(
        study_no: &str,
        body_part: &str,
        performed_at: &str,
        measurements: Vec<(&str, Option<f64>, Option<&str>)>,
    ) -> RisReport {
        RisReport {
            study_no: study_no.to_string(),
            patient_id: Some("PID-EXAMPLE".to_string()),
            body_part: Some(body_part.to_string()),
            performed_at: Some(performed_at.to_string()),
            measurements: measurements
                .into_iter()
                .map(|(item_code, value, unit)| RisReportMeasurement {
                    item_code: item_code.to_string(),
                    value,
                    unit: unit.map(|u| u.to_string()),
                })
                .collect(),
        }
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
    fn 三态一_有历史_恰好一条命中并给出差值() {
        let history = vec![report(
            "S-HIST-01",
            "kidney.left",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
        )];
        let verdict = judge(&key(), &current(32.0, "mm"), &history).expect("当前测值可用应能判定");
        assert_eq!(verdict.state, ComparisonState::HasHistory);
        assert_eq!(verdict.state.as_str(), "has_history");
        assert_eq!(verdict.hit_count, 1);
        let delta = verdict.delta.clone().expect("有历史应给出差值");
        assert_eq!(delta.delta, 4.0);
        assert_eq!(delta.direction.as_str(), "增大");
        assert!(verdict.has_delta());
        // 有历史不是失败 —— 不带失败条目。
        assert!(verdict.failure.is_none());
        assert!(verdict.is_consistent());
    }

    #[test]
    fn 三态二_无历史_判首次且不是失败() {
        let verdict = judge(&key(), &current(32.0, "mm"), &[]).expect("当前测值可用应能判定");
        assert_eq!(verdict.state, ComparisonState::FirstTime);
        assert_eq!(verdict.state.as_str(), "first_time");
        assert_eq!(verdict.hit_count, 0);
        // 首次不是失败：没有差值，也**没有失败条目**。
        assert!(verdict.delta.is_none());
        assert!(verdict.failure.is_none());
        assert!(verdict.is_first_time());
        assert!(verdict.is_consistent());
    }

    #[test]
    fn 三态三_多历史歧义_不产差值并出失败条目() {
        // **绝不任取其一** —— 取错一份历史，差值会指着错的对象。
        let history = vec![
            report(
                "S-HIST-02",
                "kidney.left",
                "2026-01-01",
                vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
            ),
            report(
                "S-HIST-03",
                "kidney.left",
                "2026-06-01",
                vec![("kidney.ap_diameter", Some(30.0), Some("mm"))],
            ),
        ];
        let verdict = judge(&key(), &current(32.0, "mm"), &history).expect("当前测值可用应能判定");
        assert_eq!(verdict.state, ComparisonState::Ambiguous);
        assert_eq!(verdict.state.as_str(), "ambiguous");
        assert_eq!(verdict.hit_count, 2);
        // 核心承诺：歧义**不产差值**。
        assert!(verdict.delta.is_none());
        assert!(!verdict.has_delta());
        // 候选逐条列出，供人工指定参照哪一份。
        assert_eq!(verdict.candidates.len(), 2);
        assert_eq!(verdict.candidates[0].study_no, "S-HIST-02");
        assert_eq!(verdict.candidates[1].study_no, "S-HIST-03");
        // 失败条目带稳定码与明细归类。
        let failure = verdict.failure.as_ref().expect("歧义必须产出失败条目");
        assert_eq!(failure.code, ErrorCode::ComparisonAmbiguous);
        assert_eq!(failure.code.as_str(), "COMPARISON_AMBIGUOUS");
        assert_eq!(
            failure.detail_code(),
            Some(DetailCode::ComparisonMultipleReports)
        );
        assert!(verdict.is_ambiguous());
        assert!(verdict.is_consistent());
    }

    #[test]
    fn 同一份报告内同测项多条也是歧义() {
        // 一份报告里同一测项出现两条 —— 参照历史同样不唯一。
        let history = vec![report(
            "S-HIST-04",
            "kidney.left",
            "2026-01-01",
            vec![
                ("kidney.ap_diameter", Some(28.0), Some("mm")),
                ("kidney.ap_diameter", Some(31.0), Some("mm")),
            ],
        )];
        let verdict = judge(&key(), &current(32.0, "mm"), &history).expect("应能判定");
        assert_eq!(verdict.state, ComparisonState::Ambiguous);
        assert!(verdict.delta.is_none());
        let failure = verdict.failure.as_ref().expect("歧义必须产出失败条目");
        assert_eq!(
            failure.detail_code(),
            Some(DetailCode::ComparisonMultipleMeasurements)
        );
        assert!(verdict.is_consistent());
    }

    #[test]
    fn 歧义失败条目指回当前测值() {
        let history = vec![
            report(
                "S-HIST-05",
                "kidney.left",
                "2026-01-01",
                vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
            ),
            report(
                "S-HIST-06",
                "kidney.left",
                "2026-06-01",
                vec![("kidney.ap_diameter", Some(30.0), Some("mm"))],
            ),
        ];
        let verdict = judge_item("PID-EXAMPLE", &item(), &history).expect("应能判定");
        let failure = verdict.failure.as_ref().expect("歧义必须产出失败条目");
        // 位置指回报告里那一条测值。
        assert_eq!(failure.line, 2);
        assert_eq!(failure.column, 6);
        assert_eq!(failure.char_start, 12);
        // 片段是**原值原单位**（3.2cm），不是归一值（32mm）。
        assert_eq!(failure.raw_snippet.as_str(), "3.2cm");
    }

    #[test]
    fn 位置未知时片段退为候选检查号且不编造位置() {
        let history = vec![
            report(
                "S-HIST-07",
                "kidney.left",
                "2026-01-01",
                vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
            ),
            report(
                "S-HIST-08",
                "kidney.left",
                "2026-06-01",
                vec![("kidney.ap_diameter", Some(30.0), Some("mm"))],
            ),
        ];
        let verdict = judge(&key(), &current(32.0, "mm"), &history).expect("应能判定");
        let failure = verdict.failure.as_ref().expect("歧义必须产出失败条目");
        // 位置未知 → 0，不编造。
        assert_eq!(failure.line, 0);
        assert_eq!(failure.column, 0);
        assert_eq!(failure.char_start, 0);
        // 片段退为候选检查号（是原文，不是说明文字）。
        assert_eq!(failure.raw_snippet.as_str(), "S-HIST-07、S-HIST-08");
    }

    #[test]
    fn 当前测值不可用时不判首次() {
        // 没比过 ≠ 没有历史 —— 不许冒充「首次」。
        let empty = CurrentMeasure {
            value_si: None,
            unit_code: None,
        };
        assert!(judge(&key(), &empty, &[]).is_none());
    }

    #[test]
    fn 状态字符串往返() {
        for state in ComparisonState::ALL {
            assert_eq!(ComparisonState::lookup(state.as_str()), Some(state));
            assert!(!state.message().is_empty());
        }
        // 「本轮没比成」是第四个状态（Task 31 落地的降级状态）。
        assert_eq!(
            ComparisonState::lookup("comparison_skipped"),
            Some(ComparisonState::Skipped)
        );
        assert_eq!(ComparisonState::lookup("no_such_state"), None);
    }

    #[test]
    fn 状态与状态库取值一致() {
        // `comparisons.state` 的取值就是这四个字符串（落库字段，跨版本不变）。
        assert_eq!(ComparisonState::HasHistory.as_str(), "has_history");
        assert_eq!(ComparisonState::FirstTime.as_str(), "first_time");
        assert_eq!(ComparisonState::Ambiguous.as_str(), "ambiguous");
        assert_eq!(ComparisonState::Skipped.as_str(), "comparison_skipped");
    }

    #[test]
    fn 歧义码落在共用码表且处置动作是待人工判定() {
        assert_eq!(
            ErrorCode::lookup("COMPARISON_AMBIGUOUS"),
            Some(ErrorCode::ComparisonAmbiguous)
        );
        let policy = ErrorCode::ComparisonAmbiguous.policy();
        assert!(!policy.blocks_report);
        assert!(!policy.drops_item);
        assert!(policy.excluded_from_comparison);
        assert_eq!(
            policy.disposition,
            crate::parse::errors::Disposition::AwaitingDisambiguation
        );
    }

    #[test]
    fn 部位键不全不进入三态判定() {
        let mut broken = item();
        broken.part_key = None;
        assert!(judge_item("PID-EXAMPLE", &broken, &[]).is_none());
    }
}
