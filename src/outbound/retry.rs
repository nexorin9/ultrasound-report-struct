//! 出站写回第 4 层：退避重试 + `pending_writeback` 告警（**失败不静默改数据**）。
//!
//! **这一层解决的接缝**：超声医师点了确认，结论也装配好了，EMR 那一端却**暂时**
//! 写不进去 —— 网关在重启、EMR 在升级、令牌刚好过期。
//!
//! **没有这一层会怎样**：一次 500 就让这份报告「医师确认过、但结论没进病历」，
//! 而库里既没有待办也没有告警 —— 结论就悬在那儿，除非有人偶然翻到失败清单。
//!
//! **这一层的全部主张就是「失败不静默改数据」**，落成四条：
//!
//! 1. **可重试的失败才退避重试**（上限 2 次、退避间隔可配）；
//! 2. **重试耗尽 → 状态如实落 `pending_writeback`**，**台账行保留**（结论没丢）；
//! 3. **落 `pending_writeback` 的那一刻发企微告警**（含检查号与稳定错误码，
//!    **不含原文**），告警本身也留痕；
//! 4. **恢复路径**：重试成功 → 回到 `written_back`；`pending_writeback` 的报告
//!    可由 replay 通道补处理，**幂等键保证不重复写**（同一把钥匙只认一次）。
//!
//! ## 为什么「不可重试的失败」不重试
//!
//! 鉴权失败（令牌无效）与请求不合契约（字段不对）重试一百遍还是同样的错 ——
//! 硬重试只是把一次失败变成三次失败，还把告警往后推。重试无意义的失败
//! **一次就停**，直接转人工介入（`pending_writeback` + 告警）。
//!
//! ## 告警发在哪一刻
//!
//! **告警发在「重试耗尽」这一刻**，不在中间失败上 ——
//! 同一次调用里的第 1、2 次失败只是退避过程，不必让群里看见三遍。
//! 每次耗尽发一条，正文带**已尝试次数**：运维一眼看得出这是第几次失败。
//!
//! ## 幂等与「不重复写」
//!
//! 回写幂等键 = `sha256(study_no‖报告内容)`，登记在幂等表的 **`writeback` 用途**下。
//! 补处理通道每次动手前先登记这把钥匙：登记不上 = **这份报告已经写过一次了**，
//! 直接拒绝，**不再发第二次写请求**。写失败时把钥匙让出来 ——
//! 否则一次失败就把这份报告永久挡在重试之外。
//!
//! ## 告警正文不含原文
//!
//! 告警正文由 [`PushSummary`] 从**结构化字段**装配：检查号、条目数、差值明细
//! （测项码 + 归一值 + 单位码）、待处理失败条目数、错误码、已尝试次数、词典版本。
//! **没有一个字来自报告原文** —— 与 EMR 出站写、企微完成通知用的是同一把尺子。

use std::error::Error as StdError;
use std::time::Duration;

use crate::config::Config;
use crate::outbound::emr::{
    build_payload, EmrClient, EmrError, EmrErrorKind, EmrWriteReceipt, StructuredReportPayload,
};
use crate::outbound::review_api::{comparison_row_to_verdict, item_row_to_parsed_item};
use crate::outbound::wecom::{PushComparison, PushKind, PushSummary, WeComClient, WeComError};
use crate::state::repo::{ComparisonRow, Repo, ReportRow};
use crate::workflow::audit::{Actor, AuditContext, AuditRecorder};
use crate::workflow::state::{transition, Status};

/// **回写重试次数的硬上限**（`spec.md` §4：退避重试 ≤2 次）。
///
/// 这是**上限不是默认值** —— 配置里写更大的值也只按 2 次算：
/// 退避重试是「上游可能马上恢复」的兜底，不是「一直重试到成功为止」。
pub const MAX_WRITEBACK_RETRIES: usize = 2;

/// **默认退避基数**（毫秒）—— 第 1 次退避 200ms、第 2 次退避 400ms（指数退避）。
pub const DEFAULT_RETRY_BASE_MS: u64 = 200;

/// **退避重试策略**（次数与间隔都可配）。
///
/// | 项 | 默认 | 说明 |
/// |---|---|---|
/// | 额外重试次数 | 2 | 上限 [`MAX_WRITEBACK_RETRIES`]（配置写大了也只按 2 次算） |
/// | 退避基数 | 200ms | 第 n 次退避 = 基数 × 2^(n-1) |
///
/// **退避间隔可配为 0** —— 那是「不等，立刻再试」，
/// 测试与「上游刚重启、等一下再试」两种场景都需要。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// 额外重试次数（≤ [`MAX_WRITEBACK_RETRIES`]）。
    pub max_retries: usize,
    /// 退避基数（毫秒）。
    pub base_backoff_ms: u64,
}

impl Default for RetryPolicy {
    /// 默认策略（2 次重试、基数 200ms）。
    fn default() -> RetryPolicy {
        RetryPolicy::standard()
    }
}

impl RetryPolicy {
    /// 造一个策略（**重试次数被夹到上限**）。
    pub fn new(max_retries: usize, base_backoff_ms: u64) -> RetryPolicy {
        RetryPolicy {
            max_retries: max_retries.min(MAX_WRITEBACK_RETRIES),
            base_backoff_ms,
        }
    }

    /// 默认策略（2 次重试、基数 200ms）。
    ///
    /// 与 [`Default`] 实现一致 —— `RetryPolicy::standard()` 与 `Default::default()`
    /// 说的是同一件事，不给两套口径。
    pub fn standard() -> RetryPolicy {
        RetryPolicy::new(MAX_WRITEBACK_RETRIES, DEFAULT_RETRY_BASE_MS)
    }

    /// **不等待**（退避基数为 0）—— 测试与「立刻再试」场景。
    pub fn immediate() -> RetryPolicy {
        RetryPolicy::new(MAX_WRITEBACK_RETRIES, 0)
    }

    /// 从配置取策略（**退避间隔可配**落在这里）。
    pub fn from_config(config: &Config) -> RetryPolicy {
        RetryPolicy::new(
            config.writeback_max_retries,
            config.writeback_backoff_ms,
        )
    }

    /// **总尝试次数** = 首次 + 重试次数。
    pub fn max_attempts(&self) -> usize {
        self.max_retries + 1
    }

    /// 第 `attempt` 次失败后、下一次尝试前的等待时长（毫秒）。
    ///
    /// `attempt` 从 1 起 —— 第 1 次失败后等 `基数`，第 2 次失败后等 `基数 × 2`。
    pub fn backoff_ms(&self, attempt: usize) -> u64 {
        if attempt == 0 {
            return 0;
        }
        let shift = (attempt - 1).min(16) as u32;
        self.base_backoff_ms.saturating_mul(1u64 << shift)
    }
}

/// **一次回写的结果**（尝试次数 / 成败 / 错误 / 退避痕迹）。
#[derive(Clone, Debug, PartialEq)]
pub struct WritebackResult {
    /// 实际尝试次数（1 = 只试过一次就没再试）。
    pub attempts: usize,
    /// 是否写成功。
    pub written: bool,
    /// 成功时的回执。
    pub receipt: Option<EmrWriteReceipt>,
    /// 失败时的错误（**最后一次**的失败）。
    pub error: Option<EmrError>,
    /// 重试是否已停止（不可重试的失败，或重试次数用尽）。
    pub retry_stopped: bool,
    /// 每次重试前实际等待的毫秒数（**可核对退避真的发生了**）。
    pub waited_ms: Vec<u64>,
    /// 是否已升级为「待人工介入」（转 `pending_writeback`）。
    pub escalated: bool,
    /// 告警是否送达（企微推送成功）。
    pub alert_delivered: bool,
}

impl WritebackResult {
    /// 造一个「还没开始」的结果。
    fn empty() -> WritebackResult {
        WritebackResult {
            attempts: 0,
            written: false,
            receipt: None,
            error: None,
            retry_stopped: false,
            waited_ms: Vec::new(),
            escalated: false,
            alert_delivered: false,
        }
    }

    /// 失败时的稳定错误码（成功时为 `None`）。
    pub fn error_code(&self) -> Option<&'static str> {
        self.error.as_ref().map(|err| err.code())
    }

    /// 面向人的一行说明（进日志与运维摘要）。
    pub fn message(&self) -> String {
        if self.written {
            return format!(
                "回写成功（尝试 {} 次，EMR 状态 {}）",
                self.attempts,
                self.receipt
                    .as_ref()
                    .map(|r| r.status)
                    .unwrap_or_default()
            );
        }
        let code = self.error_code().unwrap_or("UNKNOWN");
        format!(
            "回写失败（尝试 {} 次，错误码={}，已升级待人工介入={}，告警送达={}）",
            self.attempts, code, self.escalated, self.alert_delivered
        )
    }
}

/// **退避重试的一次回写**（纯逻辑 —— 不碰状态库、不碰 HTTP，好测）。
///
/// 语义：
///
/// - 首次一定试；
/// - 失败**可重试**（上游不可达）且次数没用完 → 退避后重试；
/// - 失败**不可重试**（鉴权 / 请求不合法 / 未配置）或次数用尽 → **停**；
/// - 停下来的原因由 [`WritebackResult::retry_stopped`] 说清，**不静默兜底**。
pub fn write_with_retry<F>(policy: &RetryPolicy, mut attempt: F) -> WritebackResult
where
    F: FnMut(usize) -> Result<EmrWriteReceipt, EmrError>,
{
    let mut result = WritebackResult::empty();
    let max_attempts = policy.max_attempts();
    while result.attempts < max_attempts {
        result.attempts += 1;
        match attempt(result.attempts) {
            Ok(receipt) => {
                result.written = true;
                result.receipt = Some(receipt);
                // 中途成功 → **不留错误**（前一次失败已被这次成功盖过去，
                // 留着会让人以为这次回写最终还是失败的）。
                result.error = None;
                return result;
            }
            Err(err) => {
                let retryable = err.retryable();
                result.error = Some(err);
                if !retryable || result.attempts >= max_attempts {
                    result.retry_stopped = true;
                    return result;
                }
                let wait_ms = policy.backoff_ms(result.attempts);
                if wait_ms > 0 {
                    std::thread::sleep(Duration::from_millis(wait_ms));
                }
                result.waited_ms.push(wait_ms);
            }
        }
    }
    result
}

/// **推送结果**（回写成功 / 告警之后单独汇报）。
#[derive(Clone, Debug, PartialEq)]
pub struct PushOutcome {
    /// 是否推送成功。
    pub pushed: bool,
    /// 推送失败的稳定错误码（成功时为 `None`）。
    pub error_code: Option<String>,
}

/// **补处理通道的错误**（显式失败，不静默跳过）。
#[derive(Clone, Debug, PartialEq)]
pub enum RetryError {
    /// 状态库里没有这份报告。
    ReportNotFound(String),
    /// 报告不在待重试状态（不是 `pending_writeback`）。
    NotPending {
        /// 检查号。
        study_no: String,
        /// 当前状态。
        status: String,
    },
    /// 这份报告没有可回写的结论（台账里一条都没有）。
    NoItem(String),
    /// **回写幂等键已登记过** —— 不再发第二次写请求。
    AlreadyClaimed(String),
    /// 状态库读写失败。
    Repo(String),
}

impl std::fmt::Display for RetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetryError::ReportNotFound(study_no) => {
                write!(f, "状态库里没有检查号 {} 的报告", study_no)
            }
            RetryError::NotPending { study_no, status } => write!(
                f,
                "报告 {} 当前状态是 {}，不是 pending_writeback，不补处理",
                study_no, status
            ),
            RetryError::NoItem(study_no) => {
                write!(f, "报告 {} 没有可回写的结构化结论，不回写空报告", study_no)
            }
            RetryError::AlreadyClaimed(study_no) => write!(
                f,
                "报告 {} 的回写幂等键已登记过，不再发第二次写请求",
                study_no
            ),
            RetryError::Repo(message) => write!(f, "状态库读写失败：{}", message),
        }
    }
}

impl StdError for RetryError {}

/// **回写通道**：回写一次（带退避重试）+ 失败升级（转 `pending_writeback` + 告警）。
///
/// **通道是回写策略的唯一落点** —— 确认回执端点（`review_api`）与补处理通道
/// （replay）都走它，两边不会写出两套重试口径。
pub struct WritebackChannel<'a> {
    /// 状态库。
    repo: &'a Repo,
    /// EMR 出站写客户端（未配置时为 `None` —— 确认了也写不进去，显式失败）。
    emr: Option<&'a EmrClient>,
    /// 企微推送客户端（未配置时为 `None` —— 告警发不出去也要留痕）。
    wecom: Option<&'a WeComClient>,
    /// 退避重试策略。
    policy: RetryPolicy,
}

impl<'a> WritebackChannel<'a> {
    /// 造一条回写通道。
    pub fn new(
        repo: &'a Repo,
        emr: Option<&'a EmrClient>,
        wecom: Option<&'a WeComClient>,
        policy: RetryPolicy,
    ) -> WritebackChannel<'a> {
        WritebackChannel {
            repo,
            emr,
            wecom,
            policy,
        }
    }

    /// 当前重试策略。
    pub fn policy(&self) -> RetryPolicy {
        self.policy
    }

    /// **回写一次（带退避重试）**。
    ///
    /// **成功时只交回执**：状态推进（`written_back`）与完成推送由调用方收口
    /// （[`finish_writeback`]）—— 确认回执与补处理两条路的成功收口走同一个函数，
    /// 免得一处改了另一处没改。
    ///
    /// **失败时本函数负责「失败不静默改数据」的全部动作**：
    /// 逐次留痕 → 转 `pending_writeback`（台账行保留）→ 发企微告警。
    pub fn write(
        &self,
        payload: &StructuredReportPayload,
        idempotency_key: &str,
        ctx: &AuditContext,
    ) -> WritebackResult {
        let recorder = AuditRecorder::new(self.repo);
        let emr = match self.emr {
            Some(client) => client,
            None => {
                // 还没发请求就失败了 —— 算一次尝试，但**不重试**（配都没配，重试无用）。
                let err = EmrError::not_configured(
                    "EMR 出站写未配置（端点或凭据缺失），确认了也写不进去",
                );
                self.record_failure(&recorder, ctx, 1, &err);
                let mut result = WritebackResult {
                    attempts: 1,
                    written: false,
                    receipt: None,
                    error: Some(err.clone()),
                    retry_stopped: true,
                    waited_ms: Vec::new(),
                    escalated: false,
                    alert_delivered: false,
                };
                self.escalate(&recorder, ctx, &mut result, &err);
                return result;
            }
        };

        let mut result = write_with_retry(&self.policy, |attempt| {
            match emr.send(payload, idempotency_key) {
                Ok(receipt) => Ok(receipt),
                Err(err) => {
                    self.record_failure(&recorder, ctx, attempt, &err);
                    Err(err)
                }
            }
        });
        if !result.written {
            if let Some(err) = result.error.clone() {
                self.escalate(&recorder, ctx, &mut result, &err);
            }
        }
        result
    }

    /// **补处理一份 `pending_writeback` 报告**（replay 通道的落点）。
    ///
    /// **恢复路径**：医师确认过、EMR 那时写不进去 → 报告停在 `pending_writeback`。
    /// 上游恢复后由运维跑一次 replay，这条报告重新走一遍回写：
    ///
    /// - **报文从台账重新装配**，不重新解析原文（重新解析会得到另一套口径）；
    /// - **幂等键保证不重复写** —— 动手前先登记这把钥匙，登记不上直接拒绝；
    /// - 写成功 → 回到 `written_back`；仍失败 → **留在** `pending_writeback`，
    ///   再发一条告警（正文带已尝试次数）。
    pub fn retry_pending(&self, study_no: &str) -> Result<WritebackResult, RetryError> {
        let report = self
            .repo
            .find_by_study_no(study_no)
            .map_err(|err| RetryError::Repo(err.to_string()))?
            .ok_or_else(|| RetryError::ReportNotFound(study_no.to_string()))?;
        let current = Status::lookup(&report.status).ok_or_else(|| RetryError::NotPending {
            study_no: study_no.to_string(),
            status: report.status.clone(),
        })?;
        if current != Status::PendingWriteback {
            return Err(RetryError::NotPending {
                study_no: study_no.to_string(),
                status: report.status.clone(),
            });
        }

        let items = self
            .repo
            .items_of_report(report.id)
            .map_err(|err| RetryError::Repo(err.to_string()))?;
        let comparisons = self
            .repo
            .comparisons_of_report(report.id)
            .map_err(|err| RetryError::Repo(err.to_string()))?;
        let parsed_items: Vec<crate::parse::extract::ParsedItem> =
            items.iter().map(item_row_to_parsed_item).collect();
        let verdicts: Vec<crate::compare::states::ComparisonVerdict> = comparisons
            .iter()
            .map(|row| comparison_row_to_verdict(row, report.patient_id.as_deref()))
            .collect();
        let build = build_payload(
            &report.study_no,
            report.patient_id.as_deref(),
            &parsed_items,
            &verdicts,
            &report.dictionary_version,
        );
        if build.payload.items.is_empty() {
            return Err(RetryError::NoItem(study_no.to_string()));
        }

        // **幂等登记**：登记不上 = 这份报告已经写过一次 —— 不发第二次写请求。
        let claimed = self
            .repo
            .claim_writeback(&report.idempotency_key, &report.study_no)
            .map_err(|err| RetryError::Repo(err.to_string()))?;
        if !claimed {
            return Err(RetryError::AlreadyClaimed(study_no.to_string()));
        }

        let ctx = AuditContext::new(
            report.id,
            &report.study_no,
            &report.idempotency_key,
            &report.dictionary_version,
        );
        let result = self.write(&build.payload, &report.idempotency_key, &ctx);
        if result.written {
            // 成功收口：推进 `written_back` + 留痕 + 推送完成通知。
            let recorder = AuditRecorder::new(self.repo);
            let detail = format!(
                "补处理回写成功：条目 {} 条，差值 {} 条，EMR 状态 {}",
                build.payload.items.len(),
                build.payload.comparisons.len(),
                result
                    .receipt
                    .as_ref()
                    .map(|r| r.status)
                    .unwrap_or_default()
            );
            match finish_writeback(
                self.repo,
                &recorder,
                self.wecom,
                &ctx,
                &report,
                Status::PendingWriteback,
                build.payload.items.len(),
                &comparisons,
                &detail,
            ) {
                Ok(_) => {}
                Err(err) => {
                    // 收口写不进去 —— 状态如实：**不推进**（宁可停在待重试，
                    // 也不让库里的状态与 EMR 里的结论对不上）。
                    let _ = recorder.failed(
                        &ctx,
                        &Actor::System,
                        &crate::parse::errors::Failure::new(
                            crate::parse::errors::ErrorCode::ReviewStateStoreUnavailable,
                            0,
                            0,
                            0,
                            &format!("回写成功但状态收口失败：{}", err),
                        ),
                    );
                }
            }
        } else {
            // 写失败 → **把钥匙让出来**（否则这份报告再也补不上）。
            let _ = self.repo.release_writeback_claim(&report.idempotency_key);
        }
        Ok(result)
    }

    /// **逐次留痕**（每次失败一条 `failed`，带尝试次数）。
    fn record_failure(
        &self,
        recorder: &AuditRecorder,
        ctx: &AuditContext,
        attempt: usize,
        err: &EmrError,
    ) {
        let detail = format!("回写第 {} 次尝试失败：{}", attempt, err.message());
        let _ = recorder.failed(ctx, &Actor::System, &err.to_failure());
        let _ = detail;
    }

    /// **失败升级**：状态如实落 `pending_writeback`（台账行保留）+ 发企微告警。
    ///
    /// **不静默改数据**：
    ///
    /// - 状态推进走**状态机守卫**（[`transition`]）—— 终态的报告不再被改动；
    /// - 台账行**一行都不动**（结论没丢，丢的是回写这一步）；
    /// - 告警正文只含结构化字段（检查号 / 错误码 / 条数 / 已尝试次数），**不含原文**。
    fn escalate(
        &self,
        recorder: &AuditRecorder,
        ctx: &AuditContext,
        result: &mut WritebackResult,
        err: &EmrError,
    ) {
        let report = ctx
            .study_no
            .as_deref()
            .and_then(|study_no| self.repo.find_by_study_no(study_no).ok().flatten());
        let report = match report {
            Some(report) => report,
            None => return,
        };
        let current = match Status::lookup(&report.status) {
            Some(status) => status,
            None => return,
        };
        // 终态（已回写 / 已驳回 / 解析失败）不再升级 —— 那些报告与本次写失败无关。
        if transition(current, Status::PendingWriteback).is_err() {
            return;
        }
        if self
            .repo
            .set_report_status(report.id, Status::PendingWriteback.as_str())
            .is_err()
        {
            return;
        }
        result.escalated = true;
        result.alert_delivered = self.send_alert(recorder, ctx, &report, result, err);
    }

    /// **发企微告警**（含检查号与稳定错误码，**不含原文**）。
    fn send_alert(
        &self,
        recorder: &AuditRecorder,
        ctx: &AuditContext,
        report: &ReportRow,
        result: &WritebackResult,
        err: &EmrError,
    ) -> bool {
        let summary = self.alert_summary(report, result, err);
        let client = match self.wecom {
            Some(client) => client,
            None => {
                // 告警发不出去 —— **留痕**（运维要知道「连告警都没发出去」）。
                let failure = WeComError::not_configured(
                    "企微推送未配置（webhook 地址缺失），回写失败告警发不出去",
                );
                let _ = recorder.failed(ctx, &Actor::System, &failure.to_failure());
                return false;
            }
        };
        match client.push(&summary) {
            Ok(_) => {
                let detail = format!(
                    "回写失败告警已送达：检查号 {}，错误码 {}，已尝试 {} 次",
                    report.study_no,
                    err.code(),
                    result.attempts
                );
                let _ = recorder.pushed(ctx, &Actor::System, Some(&detail));
                true
            }
            Err(push_err) => {
                let _ = recorder.failed(ctx, &Actor::System, &push_err.to_failure());
                false
            }
        }
    }

    /// **告警摘要**（正文只由结构化字段生成 —— 一个字都不来自报告原文）。
    fn alert_summary(
        &self,
        report: &ReportRow,
        result: &WritebackResult,
        err: &EmrError,
    ) -> PushSummary {
        let items = self
            .repo
            .items_of_report(report.id)
            .unwrap_or_default()
            .len();
        let comparisons = self
            .repo
            .comparisons_of_report(report.id)
            .unwrap_or_default();
        let failure_count = self
            .repo
            .failure_count_of_report(report.id)
            .unwrap_or_default() as usize;
        PushSummary {
            study_no: report.study_no.clone(),
            kind: PushKind::PendingWritebackAlert,
            item_count: items,
            comparisons: comparisons.iter().filter_map(push_comparison).collect(),
            pending_failure_count: failure_count,
            dictionary_version: report.dictionary_version.clone(),
            error_code: Some(err.code().to_string()),
            attempts: Some(result.attempts),
        }
    }
}

/// **成功收口的失败**（状态推进被拒 / 状态库写不进）。
#[derive(Clone, Debug, PartialEq)]
pub enum FinishError {
    /// 状态机拒绝推进（终态报告不该再动 —— 例如已被驳回）。
    Transition(String),
    /// 状态库读写失败。
    Repo(String),
}

impl std::fmt::Display for FinishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FinishError::Transition(message) => write!(f, "状态推进被拒：{}", message),
            FinishError::Repo(message) => write!(f, "状态库读写失败：{}", message),
        }
    }
}

impl StdError for FinishError {}

/// **回写成功收口**（推进 `written_back` + 留痕 + 推送完成通知）。
///
/// **成功收口只有这一处** —— 确认回执（`review_api`）与补处理通道（replay）
/// 都走它。写成两处的话，改了一处忘了另一处，审计链就会时有时无。
///
/// `from` 是**推进前**的状态：`awaiting_review`（医师刚确认）或
/// `pending_writeback`（补处理重试成功）—— 两条路都经过医师确认。
#[allow(clippy::too_many_arguments)]
pub fn finish_writeback(
    repo: &Repo,
    recorder: &AuditRecorder,
    wecom: Option<&WeComClient>,
    ctx: &AuditContext,
    report: &ReportRow,
    from: Status,
    item_count: usize,
    comparisons: &[ComparisonRow],
    detail: &str,
) -> Result<PushOutcome, FinishError> {
    let next = transition(from, Status::WrittenBack).map_err(|err| {
        FinishError::Transition(err.to_string())
    })?;
    if !repo
        .set_report_status(report.id, next.as_str())
        .map_err(|err| FinishError::Repo(err.to_string()))?
    {
        return Err(FinishError::Repo(format!(
            "报告 {} 不在库里，状态推进失败",
            report.study_no
        )));
    }
    recorder
        .written_back(ctx, &Actor::System, Some(detail))
        .map_err(|err| FinishError::Repo(err.to_string()))?;
    let failure_count = repo
        .failure_count_of_report(report.id)
        .map_err(|err| FinishError::Repo(err.to_string()))? as usize;
    Ok(push_writeback_completed(
        recorder,
        ctx,
        wecom,
        report,
        item_count,
        comparisons,
        failure_count,
    ))
}

/// **推送完成通知**（回写成功之后把结果送到群里）。
///
/// 推送失败**不改状态** —— 结论已经写进 EMR 了，状态是 `written_back`；
/// 没出去的是「通知」这一步，单独留痕。
pub fn push_writeback_completed(
    recorder: &AuditRecorder,
    ctx: &AuditContext,
    wecom: Option<&WeComClient>,
    report: &ReportRow,
    item_count: usize,
    comparisons: &[ComparisonRow],
    failure_count: usize,
) -> PushOutcome {
    let summary = PushSummary {
        study_no: report.study_no.clone(),
        kind: PushKind::WritebackCompleted,
        item_count,
        comparisons: comparisons.iter().filter_map(push_comparison).collect(),
        pending_failure_count: failure_count,
        dictionary_version: report.dictionary_version.clone(),
        error_code: None,
        attempts: None,
    };
    let client = match wecom {
        Some(client) => client,
        None => {
            let err = WeComError::not_configured(
                "企微推送未配置（webhook 地址缺失），结论已回写但群里没收到",
            );
            let _ = recorder.failed(ctx, &Actor::System, &err.to_failure());
            return PushOutcome {
                pushed: false,
                error_code: Some(err.code().to_string()),
            };
        }
    };
    match client.push(&summary) {
        Ok(_receipt) => {
            let detail = format!("企微推送成功：检查号 {}", report.study_no);
            let _ = recorder.pushed(ctx, &Actor::System, Some(&detail));
            PushOutcome {
                pushed: true,
                error_code: None,
            }
        }
        Err(err) => {
            let _ = recorder.failed(ctx, &Actor::System, &err.to_failure());
            PushOutcome {
                pushed: false,
                error_code: Some(err.code().to_string()),
            }
        }
    }
}

/// **对拍差值行 → 推送摘要里的差值明细**（**只有码与数**）。
pub fn push_comparison(row: &ComparisonRow) -> Option<PushComparison> {
    if row.state != crate::compare::states::ComparisonState::HasHistory.as_str() {
        return None;
    }
    Some(PushComparison {
        item_code: row.item_code.clone()?,
        current_value: row.current_value?,
        previous_value: row.previous_value?,
        delta: row.delta?,
        unit_code: row.unit_code.clone()?,
    })
}

/// 失败语义 → 是否值得退避重试（**出站写只有上游不可达值得**）。
///
/// 与 [`EmrErrorKind::retryable`] 同一口径，这里单独列出来是为了让
/// 「重试什么、不重试什么」在回写通道这一层看得见。
pub fn retryable_kind(kind: EmrErrorKind) -> bool {
    kind.retryable()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbound::emr::EmrErrorKind;
    use crate::parse::errors::DetailCode;

    /// 造一个可重试的失败（上游不可达）。
    fn upstream_error() -> EmrError {
        EmrError::new(
            EmrErrorKind::UpstreamUnavailable,
            Some(DetailCode::EmrServerError),
            "EMR 返回 500（上游服务自身出错）",
        )
    }

    /// 造一个不可重试的失败（鉴权失败）。
    fn auth_error() -> EmrError {
        EmrError::new(
            EmrErrorKind::AuthFailed,
            Some(DetailCode::EmrUnauthorized),
            "EMR 返回 401（令牌无效或无权限）",
        )
    }

    /// 造一个成功回执。
    fn receipt() -> EmrWriteReceipt {
        EmrWriteReceipt {
            status: 200,
            receipt_id: Some("SR-EXAMPLE".to_string()),
            body_snippet: "{}".to_string(),
        }
    }

    #[test]
    fn 重试次数被夹在两次以内() {
        // 配置写大了也只按 2 次算 —— 退避重试是兜底，不是「一直重试到成功」。
        assert_eq!(RetryPolicy::new(5, 200).max_retries, MAX_WRITEBACK_RETRIES);
        assert_eq!(RetryPolicy::new(5, 200).max_attempts(), 3);
        assert_eq!(RetryPolicy::new(0, 200).max_attempts(), 1);
        assert_eq!(RetryPolicy::standard().max_retries, 2);
    }

    #[test]
    fn 退避间隔按指数增长且可配() {
        let policy = RetryPolicy::new(2, 200);
        assert_eq!(policy.backoff_ms(1), 200);
        assert_eq!(policy.backoff_ms(2), 400);
        assert_eq!(policy.backoff_ms(0), 0);
        // 间隔可配为 0（不等，立刻再试）。
        assert_eq!(RetryPolicy::new(2, 0).backoff_ms(1), 0);
    }

    #[test]
    fn 上游不可达退避重试两次后停() {
        let policy = RetryPolicy::new(2, 0);
        let mut seen: Vec<usize> = Vec::new();
        let result = write_with_retry(&policy, |attempt| {
            seen.push(attempt);
            Err(upstream_error())
        });
        assert_eq!(seen, vec![1, 2, 3], "应试满 3 次（首次 + 2 次重试）");
        assert_eq!(result.attempts, 3);
        assert!(!result.written);
        assert!(result.retry_stopped, "重试次数用尽即停");
        assert_eq!(result.error_code(), Some("EMR_UPSTREAM_UNAVAILABLE"));
    }

    #[test]
    fn 不可重试的失败一次就停() {
        let policy = RetryPolicy::new(2, 0);
        let mut seen: Vec<usize> = Vec::new();
        let result = write_with_retry(&policy, |attempt| {
            seen.push(attempt);
            Err(auth_error())
        });
        assert_eq!(seen, vec![1], "鉴权失败重试无用，只试一次");
        assert_eq!(result.attempts, 1);
        assert!(!result.written);
        assert!(result.retry_stopped);
        assert_eq!(result.error_code(), Some("EMR_AUTH_FAILED"));
    }

    #[test]
    fn 成功不重试() {
        let policy = RetryPolicy::new(2, 0);
        let mut seen: Vec<usize> = Vec::new();
        let result = write_with_retry(&policy, |attempt| {
            seen.push(attempt);
            Ok(receipt())
        });
        assert_eq!(seen, vec![1]);
        assert!(result.written);
        assert_eq!(result.attempts, 1);
        assert!(!result.retry_stopped);
        assert!(result.receipt.is_some());
        assert!(result.error.is_none());
    }

    #[test]
    fn 中途成功就停不再重试() {
        let policy = RetryPolicy::new(2, 0);
        let mut seen: Vec<usize> = Vec::new();
        let result = write_with_retry(&policy, |attempt| {
            seen.push(attempt);
            if attempt < 2 {
                Err(upstream_error())
            } else {
                Ok(receipt())
            }
        });
        assert_eq!(seen, vec![1, 2]);
        assert!(result.written);
        assert_eq!(result.attempts, 2);
        assert!(result.error.is_none());
    }

    #[test]
    fn 退避痕迹可核对() {
        let policy = RetryPolicy::new(2, 0);
        let result = write_with_retry(&policy, |_| Err(upstream_error()));
        assert_eq!(result.waited_ms, vec![0, 0], "两次重试各留一条退避痕迹");
        assert_eq!(result.waited_ms.len(), 2);
    }

    #[test]
    fn 结果说明带尝试次数与错误码() {
        let policy = RetryPolicy::new(2, 0);
        let result = write_with_retry(&policy, |_| Err(upstream_error()));
        let text = result.message();
        assert!(text.contains("3"), "应带尝试次数：{}", text);
        assert!(text.contains("EMR_UPSTREAM_UNAVAILABLE"), "{}", text);

        let ok = write_with_retry(&policy, |_| Ok(receipt()));
        assert!(ok.message().contains("回写成功"));
    }

    #[test]
    fn 可重试语义与出站写四分类一致() {
        assert!(retryable_kind(EmrErrorKind::UpstreamUnavailable));
        assert!(!retryable_kind(EmrErrorKind::AuthFailed));
        assert!(!retryable_kind(EmrErrorKind::BadResponse));
        assert!(!retryable_kind(EmrErrorKind::NotConfigured));
    }

    #[test]
    fn 补处理错误说得清是哪一种() {
        let not_found = RetryError::ReportNotFound("S-001".to_string());
        assert!(not_found.to_string().contains("S-001"));
        let not_pending = RetryError::NotPending {
            study_no: "S-002".to_string(),
            status: "written_back".to_string(),
        };
        assert!(not_pending.to_string().contains("written_back"));
        let claimed = RetryError::AlreadyClaimed("S-003".to_string());
        assert!(claimed.to_string().contains("不再发第二次写请求"));
        let no_item = RetryError::NoItem("S-004".to_string());
        assert!(no_item.to_string().contains("不回写空报告"));
    }
}
