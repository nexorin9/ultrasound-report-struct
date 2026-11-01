//! 内置确认回执端点 —— **人工确认点**（本产品唯一的人工写入口）。
//!
//! **这一层解决的接缝**：解析内核把测值算出来、对拍引擎把差值算出来之后，
//! 结论还只是**一份草稿** —— 它没有进 EMR，群里也还没人知道。
//! 这一层就是那个「谁来点头、点了之后怎么落地」的落点。
//!
//! ## 接口契约（与 `spec.md` §2.3「入站回执」一行一致）
//!
//! ```text
//! POST /api/v1/reports/{study_no}/review
//! Authorization: Bearer $DAEMON_API_TOKEN
//! Content-Type: application/json
//!
//! {"decision":"confirm"|"reject", "reviewer":"USR-SONO-EXAMPLE", "note":"可选备注"}
//! ```
//!
//! ## 为什么是「内置」而不是「让医师去点 EMR 界面」
//!
//! 医师看的是**工作站上那份报告**，不是本产品的台账。让他在另一个界面里
//! 再找一遍检查号、再点一次「确认」，等于把「抄漏了没人知道」换成
//! 「确认了没人知道」。所以回执端点挂在**守护进程**上：报告到了、台账落了，
//! 医师在工作站上看到这份报告，回执就打到这里。
//!
//! ## 不自动回写（这一层最要紧的一条）
//!
//! **回写 EMR 的调用点只有一处** —— [`ReviewHandler::apply_review`] 里
//! `decision=confirm` 的那一支。守护进程解析完、台账落完了、差值算完了，
//! **都不会**发出任何一次 EMR 写请求 —— 没收到 `confirm` 就没有写请求。
//!
//! 状态机是这条不变量的第二道保险：进 `written_back` 的来路只有
//! `awaiting_review`（医师确认过）与 `pending_writeback`（重试成功，
//! 而它本身也来自 `awaiting_review`）—— **没有任何一条路能绕过医师确认**。
//!
//! ## 凭据只从环境变量读
//!
//! `DAEMON_API_TOKEN` 只走环境变量；配置文件里出现密钥形状的键**启动即拒**
//! （见 `config.rs`）。令牌比对用**定长比较**，不因「早一个字符返回」而泄露长度。
//!
//! ## 失败不另造一套码
//!
//! 回执端点的失败收敛到**全项目共用的那张稳定码表**
//! （[`crate::parse::errors`]）—— 运维按错误码聚合时，
//! 「医师的确认没走通」和「测值缺单位」在同一张表里。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::compare::{ComparisonKey, ComparisonState, ComparisonVerdict, Delta, MeasureAnchor};
use crate::config::{Config, Credentials};
use crate::outbound::emr::{build_payload, EmrClient, EmrError, EmrErrorKind};
use crate::outbound::retry::{finish_writeback, RetryPolicy, WritebackChannel};
use crate::outbound::wecom::WeComClient;
use crate::parse::errors::{DetailCode, ErrorCode, Failure};
use crate::parse::extract::ParsedItem;
use crate::state::repo::{ComparisonRow, ItemRow, Repo, RepoError};
use crate::workflow::audit::{Actor, AuditContext, AuditRecorder};
use crate::workflow::state::{transition, ReviewDecision, Status};

/// 回执端点的路由前缀（`{study_no}` 夹在中间）。
pub const REVIEW_ROUTE_PREFIX: &str = "/api/v1/reports/";

/// 回执端点的路由后缀。
pub const REVIEW_ROUTE_SUFFIX: &str = "/review";

/// 请求体大小上限（字节）—— 回执体只有决定值 / 确认人 / 备注，
/// 不该有大报文；超了直接拒（不让一条坏报文把内存吃光）。
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// 读超时（秒）—— 医师工作站点一次「确认」，端点不该把连接吊着不放。
const READ_TIMEOUT_SECS: u64 = 15;

/// 一次回执请求（**已解析成结构**）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReviewRequest {
    /// HTTP 方法。
    pub method: String,
    /// 请求路径（含查询串，本端点不关心查询串）。
    pub path: String,
    /// 请求头（`Content-Length` 与 `Authorization` 在这里）。
    pub headers: Vec<(String, String)>,
    /// 请求体（JSON 文本）。
    pub body: String,
}

impl ReviewRequest {
    /// 造一条回执请求（装配与测试用）。
    ///
    /// `token` 为 `None` 时**不带** `Authorization` 头 —— 用来验证
    /// 「缺令牌被拒」这条路径。
    pub fn new(method: &str, path: &str, token: Option<&str>, body: &str) -> ReviewRequest {
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        if let Some(token) = token {
            headers.push(("Authorization".to_string(), format!("Bearer {}", token)));
        }
        ReviewRequest {
            method: method.to_string(),
            path: path.to_string(),
            headers,
            body: body.to_string(),
        }
    }

    /// 取请求头（**大小写不敏感** —— HTTP 头名不区分大小写）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// 一次回执的应答。
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 响应体（JSON 文本）。
    pub body: String,
}

impl ReviewResponse {
    /// 造一个 JSON 应答。
    fn json(status: u16, value: &serde_json::Value) -> ReviewResponse {
        ReviewResponse {
            status,
            body: value.to_string(),
        }
    }

    /// 响应体解析成 JSON（解析不了返回 `None`）。
    pub fn value(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.body).ok()
    }

    /// 应答里的稳定错误码（成功应答没有这一位）。
    pub fn error_code(&self) -> Option<ErrorCode> {
        let value = self.value()?;
        let raw = value.get("error_code")?.as_str()?;
        ErrorCode::lookup(raw)
    }

    /// 应答里的明细归类。
    pub fn error_detail(&self) -> Option<DetailCode> {
        let value = self.value()?;
        let raw = value.get("detail")?.as_str()?;
        DetailCode::lookup(raw)
    }

    /// 应答里的失败条目（**位置一律 0** —— 回执端点的失败不在报告正文里，
    /// **不编造行列**）。
    ///
    /// 成功应答返回 `None`。
    pub fn failure(&self) -> Option<Failure> {
        let value = self.value()?;
        let code = ErrorCode::lookup(value.get("error_code")?.as_str()?)?;
        let detail = value
            .get("detail")
            .and_then(|raw| raw.as_str())
            .and_then(DetailCode::lookup);
        let reason = value
            .get("reason")
            .and_then(|raw| raw.as_str())
            .unwrap_or(code.message());
        let mut failure = Failure::new(code, 0, 0, 0, reason);
        if let Some(detail) = detail {
            failure = failure.with_detail_code(detail);
        }
        Some(failure)
    }
}

/// 回执端点的**请求级失败**（进响应体，也进审计 / 失败清单）。
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewError {
    /// 稳定错误码（全项目共用码表里的）。
    pub code: ErrorCode,
    /// 明细归类。
    pub detail: Option<DetailCode>,
    /// HTTP 状态码。
    pub status: u16,
    /// 中文原因（进响应体与失败清单）。
    pub reason: String,
}

impl ReviewError {
    /// 造一条回执失败。
    fn new(
        code: ErrorCode,
        detail: Option<DetailCode>,
        status: u16,
        reason: impl Into<String>,
    ) -> ReviewError {
        ReviewError {
            code,
            detail,
            status,
            reason: reason.into(),
        }
    }

    /// 面向人的说明：稳定码 + 中文文案 + 明细归类 + 原因。
    pub fn message(&self) -> String {
        let mut text = format!("错误码={} 说明={}", self.code.as_str(), self.code.message());
        if let Some(detail) = self.detail {
            text.push_str(&format!("（{}）", detail.message()));
        }
        text.push_str(&format!(" 原因={}", self.reason));
        text
    }

    /// 转成**共用**失败条目（进失败清单 / 审计）。
    ///
    /// 位置一律 0 —— 回执端点的失败不在报告正文里，**不编造行列**。
    pub fn to_failure(&self) -> Failure {
        let mut failure = Failure::new(self.code, 0, 0, 0, &self.reason);
        if let Some(detail) = self.detail {
            failure = failure.with_detail_code(detail);
        }
        failure
    }

    /// 转成应答（响应体带稳定码 / 明细 / 中文说明）。
    pub fn to_response(&self) -> ReviewResponse {
        ReviewResponse::json(
            self.status,
            &serde_json::json!({
                "error_code": self.code.as_str(),
                "detail": self.detail.map(|d| d.as_str()),
                "message": self.code.message(),
                "reason": self.reason,
            }),
        )
    }
}

/// 回执请求体（`spec.md` §2.3 的字段规范）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReviewBody {
    /// 决定：`confirm` / `reject`。
    #[serde(default)]
    pub decision: String,
    /// 确认人账号（审计链要说得清「这一步是谁做的」）。
    #[serde(default)]
    pub reviewer: String,
    /// 备注（可选）。
    #[serde(default)]
    pub note: Option<String>,
}

/// **回执处理器**（端点的业务逻辑，与传输层分开）。
///
/// 传输层（HTTP/1.1 解析与应答）只做「把字节变成请求、把结果变成响应」；
/// 这一层只做业务判断 —— 所以业务路径**可以脱离 socket 直接测**。
pub struct ReviewHandler {
    /// 状态库路径（每个请求开一条连接 —— SQLite 自己串行化写入）。
    state_db: PathBuf,
    /// 端点令牌（`DAEMON_API_TOKEN`）。
    token: String,
    /// EMR 出站写客户端（未配置时为 `None` —— 确认会显式失败，不静默跳过）。
    emr: Option<EmrClient>,
    /// 企微推送客户端（未配置时为 `None`）。
    wecom: Option<WeComClient>,
    /// **回写退避重试策略**（次数与间隔可配 —— `src/outbound/retry.rs`）。
    policy: RetryPolicy,
}

impl ReviewHandler {
    /// 造一个处理器（用默认退避策略：重试 ≤2 次、基数 200ms）。
    pub fn new(
        state_db: PathBuf,
        token: String,
        emr: Option<EmrClient>,
        wecom: Option<WeComClient>,
    ) -> ReviewHandler {
        ReviewHandler {
            state_db,
            token,
            emr,
            wecom,
            policy: RetryPolicy::standard(),
        }
    }

    /// 造一个处理器并**指定退避重试策略**（退避间隔可配）。
    pub fn with_policy(
        state_db: PathBuf,
        token: String,
        emr: Option<EmrClient>,
        wecom: Option<WeComClient>,
        policy: RetryPolicy,
    ) -> ReviewHandler {
        ReviewHandler {
            state_db,
            token,
            emr,
            wecom,
            policy,
        }
    }

    /// 从配置 + 凭据造处理器（**凭据只从环境变量读**；退避策略取配置）。
    pub fn from_config(config: &Config, creds: &Credentials) -> ReviewHandler {
        let emr = EmrClient::from_config(config, creds).ok();
        let wecom = WeComClient::from_config(config, creds).ok();
        ReviewHandler {
            state_db: config.state_db.clone(),
            token: creds.daemon_api_token.clone().unwrap_or_default(),
            emr,
            wecom,
            policy: RetryPolicy::from_config(config),
        }
    }

    /// 处理一条回执请求（**端点的唯一入口**）。
    pub fn handle(&self, request: &ReviewRequest) -> ReviewResponse {
        match self.route(request) {
            Ok(target) => self.apply_review(&target),
            Err(err) => err.to_response(),
        }
    }

    /// **路由与鉴权**（业务逻辑之前的那道门）。
    ///
    /// 顺序：路径 → 方法 → 令牌 → 请求体。
    /// **先看路径再鉴权**：路径不对就是「没这个端点」，没什么好鉴权的；
    /// 路径对了才问令牌 —— 免得 unauthenticated 的调用方把路由表探一遍。
    fn route(&self, request: &ReviewRequest) -> Result<ReviewTarget, ReviewError> {
        let study_no = match parse_route(&request.path) {
            Some(study_no) => study_no,
            None => {
                return Err(ReviewError::new(
                    ErrorCode::ReviewNotFound,
                    Some(DetailCode::ReviewRouteNotFound),
                    404,
                    format!(
                        "路径 {} 不匹配回执端点（{}<study_no>{}）",
                        request.path, REVIEW_ROUTE_PREFIX, REVIEW_ROUTE_SUFFIX
                    ),
                ))
            }
        };
        if !request.method.eq_ignore_ascii_case("POST") {
            return Err(ReviewError::new(
                ErrorCode::ReviewBadRequest,
                Some(DetailCode::ReviewMethodNotAllowed),
                405,
                format!(
                    "回执端点只收 POST，收到的是 {}",
                    request.method
                ),
            ));
        }
        // 令牌：缺头与令牌不对**分开记**（运维要分清是没带还是带错了）。
        let header = match request.header("Authorization") {
            Some(value) => value.trim(),
            None => {
                return Err(ReviewError::new(
                    ErrorCode::ReviewUnauthorized,
                    Some(DetailCode::ReviewTokenMissing),
                    401,
                    "缺 Authorization 头（回执端点要求 Bearer 令牌）",
                ))
            }
        };
        let presented = match header.strip_prefix("Bearer ") {
            Some(token) => token.trim(),
            None => {
                return Err(ReviewError::new(
                    ErrorCode::ReviewUnauthorized,
                    Some(DetailCode::ReviewTokenInvalid),
                    401,
                    "Authorization 头不是 Bearer 令牌格式",
                ))
            }
        };
        if self.token.is_empty() {
            return Err(ReviewError::new(
                ErrorCode::ReviewUnauthorized,
                Some(DetailCode::ReviewTokenInvalid),
                401,
                "端点未配置令牌（DAEMON_API_TOKEN 未设置），一律拒绝",
            ));
        }
        if !constant_time_eq(presented.as_bytes(), self.token.as_bytes()) {
            return Err(ReviewError::new(
                ErrorCode::ReviewUnauthorized,
                Some(DetailCode::ReviewTokenInvalid),
                401,
                "Bearer 令牌不对",
            ));
        }

        // 请求体：必须是 JSON，且决定值必须是 confirm / reject。
        let parsed: ReviewBody = match serde_json::from_str(&request.body) {
            Ok(body) => body,
            Err(err) => {
                return Err(ReviewError::new(
                    ErrorCode::ReviewBadRequest,
                    Some(DetailCode::ReviewBodyNotJson),
                    400,
                    format!("请求体不是合法 JSON：{}", err),
                ))
            }
        };
        let decision = match ReviewDecision::lookup(parsed.decision.trim()) {
            Some(decision) => decision,
            None => {
                return Err(ReviewError::new(
                    ErrorCode::ReviewBadRequest,
                    Some(DetailCode::ReviewDecisionInvalid),
                    400,
                    format!(
                        "decision 取值 {:?} 不是 confirm / reject（不猜默认动作）",
                        parsed.decision
                    ),
                ))
            }
        };
        let reviewer = parsed.reviewer.trim().to_string();
        if reviewer.is_empty() {
            return Err(ReviewError::new(
                ErrorCode::ReviewBadRequest,
                Some(DetailCode::ReviewReviewerMissing),
                400,
                "缺确认人（审计链要说得清这一步是谁做的）",
            ));
        }

        Ok(ReviewTarget {
            study_no,
            decision,
            reviewer,
            note: parsed.note,
        })
    }

    /// **落一次回执**（确认 → 回写 + 推送；驳回 → 终态）。
    fn apply_review(&self, target: &ReviewTarget) -> ReviewResponse {
        let repo = match Repo::open(&self.state_db) {
            Ok(repo) => repo,
            Err(err) => {
                return ReviewError::new(
                    ErrorCode::ReviewStateStoreUnavailable,
                    None,
                    500,
                    format!("状态库打不开（{}）：{}", self.state_db.display(), err),
                )
                .to_response()
            }
        };
        let report = match repo.find_by_study_no(&target.study_no) {
            Ok(Some(row)) => row,
            Ok(None) => {
                return ReviewError::new(
                    ErrorCode::ReviewNotFound,
                    Some(DetailCode::ReviewReportNotFound),
                    404,
                    format!("状态库里没有检查号 {} 的报告", target.study_no),
                )
                .to_response()
            }
            Err(err) => {
                return ReviewError::new(
                    ErrorCode::ReviewStateStoreUnavailable,
                    None,
                    500,
                    format!("读状态库失败：{}", err),
                )
                .to_response()
            }
        };

        // 状态守卫：**只有待确认的草稿才谈得上确认**。
        //
        // `parsed` 的报告先进入人工确认队列（`parsed → awaiting_review`）——
        // 这一步由状态机守卫把关，不在这里手写判断。
        let current = match Status::lookup(&report.status) {
            Some(Status::Parsed) => {
                match transition(Status::Parsed, Status::AwaitingReview) {
                    Ok(next) => {
                        if let Err(err) = repo.set_report_status(report.id, next.as_str()) {
                            return ReviewError::new(
                                ErrorCode::ReviewStateStoreUnavailable,
                                None,
                                500,
                                format!("推进报告状态失败：{}", err),
                            )
                            .to_response();
                        }
                        next
                    }
                    Err(err) => return err_response(err),
                }
            }
            Some(status) if status.awaits_confirmation() => status,
            Some(other) => {
                return ReviewError::new(
                    ErrorCode::ReviewIllegalState,
                    Some(DetailCode::ReviewStateNotReviewable),
                    409,
                    format!(
                        "报告当前状态是 {}（{}），不能再推进确认",
                        other.as_str(),
                        other.message()
                    ),
                )
                .to_response()
            }
            None => {
                return ReviewError::new(
                    ErrorCode::ReviewIllegalState,
                    Some(DetailCode::ReviewStateNotReviewable),
                    409,
                    format!("报告状态 {:?} 不在状态机取值集合里", report.status),
                )
                .to_response()
            }
        };

        let recorder = AuditRecorder::new(&repo);
        let actor = Actor::user(target.reviewer.clone());
        // 摘要哈希与幂等键同源：库里存的幂等键就是 `sha256(study_no‖报告内容)`。
        let ctx = AuditContext::new(
            report.id,
            &report.study_no,
            &report.idempotency_key,
            &report.dictionary_version,
        );

        match target.decision {
            ReviewDecision::Reject => self.apply_reject(&repo, &recorder, &ctx, &actor, target, current),
            ReviewDecision::Confirm => {
                self.apply_confirm(&repo, &recorder, &ctx, &actor, target, current, &report)
            }
        }
    }

    /// **驳回**：落终态 `rejected`，**不回写、不推送**。
    ///
    /// 驳回是医师说「这份结论不对」—— 把它写进 EMR 才是事故。
    fn apply_reject(
        &self,
        repo: &Repo,
        recorder: &AuditRecorder,
        ctx: &AuditContext,
        actor: &Actor,
        target: &ReviewTarget,
        current: Status,
    ) -> ReviewResponse {
        match transition(current, Status::Rejected) {
            Ok(next) => {
                if let Err(err) = repo.set_report_status(report_id(ctx), next.as_str()) {
                    return ReviewError::new(
                        ErrorCode::ReviewStateStoreUnavailable,
                        None,
                        500,
                        format!("推进报告状态失败：{}", err),
                    )
                    .to_response();
                }
                let detail = review_detail(target);
                if let Err(err) = recorder.rejected(ctx, actor, Some(&detail)) {
                    return ReviewError::new(
                        ErrorCode::ReviewStateStoreUnavailable,
                        None,
                        500,
                        format!("写审计留痕失败：{}", err),
                    )
                    .to_response();
                }
                ReviewResponse::json(
                    200,
                    &serde_json::json!({
                        "study_no": target.study_no,
                        "decision": target.decision.as_str(),
                        "status": next.as_str(),
                        "status_message": next.message(),
                        "reviewer": target.reviewer,
                        "emr_written": false,
                    }),
                )
            }
            Err(err) => err_response(err),
        }
    }

    /// **确认**：回写 EMR + 推送企微 → 落终态 `written_back`。
    #[allow(clippy::too_many_arguments)]
    fn apply_confirm(
        &self,
        repo: &Repo,
        recorder: &AuditRecorder,
        ctx: &AuditContext,
        actor: &Actor,
        target: &ReviewTarget,
        current: Status,
        report: &crate::state::repo::ReportRow,
    ) -> ReviewResponse {
        // 1) 读回台账里的条目与差值 —— **确认的正是台账里那一套口径**，
        //    不重新解析原文（重新解析会得到另一套口径）。
        let items = match repo.items_of_report(report.id) {
            Ok(items) => items,
            Err(err) => {
                return ReviewError::new(
                    ErrorCode::ReviewStateStoreUnavailable,
                    None,
                    500,
                    format!("读台账条目失败：{}", err),
                )
                .to_response()
            }
        };
        let comparisons = match repo.comparisons_of_report(report.id) {
            Ok(rows) => rows,
            Err(err) => {
                return ReviewError::new(
                    ErrorCode::ReviewStateStoreUnavailable,
                    None,
                    500,
                    format!("读对拍差值失败：{}", err),
                )
                .to_response()
            }
        };

        // 2) 装配外发报文（**唯一装配路径**：走 `emr::build_payload`，
        //    报文里没有放原文的地方 —— 那是「外发只发结构化结论」的结构保证）。
        let parsed_items: Vec<ParsedItem> = items.iter().map(item_row_to_parsed_item).collect();
        let verdicts: Vec<ComparisonVerdict> = comparisons
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
            // 一条结论都没有还往 EMR 里写 —— 只会多出一张空报告。
            return ReviewError::new(
                ErrorCode::ReviewIllegalState,
                Some(DetailCode::ReviewNoItem),
                409,
                format!(
                    "这份报告没有可回写的结构化结论（跳过 {} 条），不回写空报告",
                    build.skipped_items
                ),
            )
            .to_response();
        }

        // 3) 幂等登记（用途 `writeback`）—— 同一份报告**不产生第二次回写**。
        //    登记在发请求之前：万一上一次写成功了、状态没落上，
        //    这一把钥匙也已经登记过了，不会再写一次。
        match repo.claim_writeback(&report.idempotency_key, &report.study_no) {
            Ok(true) => {}
            Ok(false) => {
                return ReviewError::new(
                    ErrorCode::ReviewIllegalState,
                    Some(DetailCode::ReviewStateNotReviewable),
                    409,
                    "这份报告的回写已经登记过了，不再发第二次写请求",
                )
                .to_response()
            }
            Err(err) => {
                return ReviewError::new(
                    ErrorCode::ReviewStateStoreUnavailable,
                    None,
                    500,
                    format!("登记回写幂等键失败：{}", err),
                )
                .to_response()
            }
        }

        // 4) 医师确认留痕（**先留痕再动作** —— 动作失败时，
        //    审计链上仍然看得见「医师点过确认」）。
        let detail = review_detail(target);
        if let Err(err) = recorder.confirmed(ctx, actor, Some(&detail)) {
            release_claim(repo, &report.idempotency_key);
            return ReviewError::new(
                ErrorCode::ReviewStateStoreUnavailable,
                None,
                500,
                format!("写审计留痕失败：{}", err),
            )
            .to_response();
        }

        // 5) 出站写 EMR（**走回写通道**：退避重试 + 失败升级）。
        //
        //    通道负责「失败不静默改数据」的全部动作：逐次留痕 → 转 `pending_writeback`
        //    （台账行保留）→ 发企微告警。成功时只交回执，状态收口在下一步统一做
        //    （与补处理通道共用同一个收口函数）。
        let channel = WritebackChannel::new(
            repo,
            self.emr.as_ref(),
            self.wecom.as_ref(),
            self.policy,
        );
        let result = channel.write(&build.payload, &report.idempotency_key, ctx);
        let receipt = match result.receipt {
            Some(receipt) => receipt,
            None => {
                // 写失败（含重试耗尽）→ **状态已如实落 `pending_writeback`**，
                // 台账行保留（结论没丢），告警已发。
                // 把幂等钥匙让出来 —— 否则补处理通道再也登记不上这把钥匙。
                release_claim(repo, &report.idempotency_key);
                let err = match result.error {
                    Some(err) => err,
                    None => EmrError::new(
                        EmrErrorKind::BadResponse,
                        None,
                        "回写未产出结果（既没有回执也没有错误）",
                    ),
                };
                // 未配置 = 503（还没发请求就没了）；发了请求还失败 = 502（上游那一端的锅）。
                let status = if err.kind == EmrErrorKind::NotConfigured {
                    503
                } else {
                    502
                };
                return ReviewError::new(err.error_code(), err.detail, status, err.reason.clone())
                    .to_response()
                    .with_status_note(Status::PendingWriteback.as_str());
            }
        };

        // 6) 写成功 → 落终态 `written_back`（收口只有一处：与补处理通道共用）。
        let write_detail = format!(
            "回写成功（确认人={}）：条目 {} 条，差值 {} 条，EMR 状态 {}",
            target.reviewer,
            build.payload.items.len(),
            build.payload.comparisons.len(),
            receipt.status
        );
        match finish_writeback(
            repo,
            recorder,
            self.wecom.as_ref(),
            ctx,
            report,
            current,
            build.payload.items.len(),
            &comparisons,
            &write_detail,
        ) {
            Ok(push) => ReviewResponse::json(
                200,
                &serde_json::json!({
                    "study_no": target.study_no,
                    "decision": target.decision.as_str(),
                    "status": Status::WrittenBack.as_str(),
                    "status_message": Status::WrittenBack.message(),
                    "reviewer": target.reviewer,
                    "item_count": build.payload.items.len(),
                    "comparison_count": build.payload.comparisons.len(),
                    "skipped_items": build.skipped_items,
                    "emr_written": true,
                    "emr_status": receipt.status,
                    "emr_attempts": result.attempts,
                    "wecom_pushed": push.pushed,
                    "wecom_error_code": push.error_code,
                }),
            ),
            Err(err) => {
                release_claim(repo, &report.idempotency_key);
                ReviewError::new(
                    ErrorCode::ReviewStateStoreUnavailable,
                    None,
                    500,
                    format!("回写成功但状态收口失败：{}", err),
                )
                .to_response()
            }
        }
    }
}

/// 一次回执的**目标**（路由与鉴权之后拿到的东西）。
#[derive(Clone, Debug, PartialEq)]
struct ReviewTarget {
    /// 检查号。
    study_no: String,
    /// 决定。
    decision: ReviewDecision,
    /// 确认人账号。
    reviewer: String,
    /// 备注。
    note: Option<String>,
}

// 推送结果（回写成功之后单独汇报）复用 `outbound::retry` 的那一份 ——
// 成功收口与告警推送都在回写通道里，两处各写一份必然会漂移。
impl ReviewResponse {
    /// 给错误应答补上**报告当前状态**（调用方据此判断要不要重发）。
    fn with_status_note(self, status: &str) -> ReviewResponse {
        match self.value() {
            Some(mut value) => {
                if let Some(object) = value.as_object_mut() {
                    object.insert(
                        "status".to_string(),
                        serde_json::Value::String(status.to_string()),
                    );
                }
                ReviewResponse {
                    status: self.status,
                    body: value.to_string(),
                }
            }
            None => self,
        }
    }
}

/// 状态机拒绝迁移时的应答（**不静默忽略** —— 非法迁移必须说得清）。
fn err_response(err: crate::workflow::state::TransitionError) -> ReviewResponse {
    ReviewError::new(
        ErrorCode::ReviewIllegalState,
        Some(DetailCode::ReviewStateNotReviewable),
        409,
        err.to_string(),
    )
    .to_response()
}

/// 回执留痕的详情（**人读的一行**）。
fn review_detail(target: &ReviewTarget) -> String {
    match &target.note {
        Some(note) if !note.trim().is_empty() => {
            format!("决定={}；确认人={}；备注={}", target.decision.as_str(), target.reviewer, note.trim())
        }
        _ => format!("决定={}；确认人={}", target.decision.as_str(), target.reviewer),
    }
}

/// 台账条目行 → 解析条目（回写装配用）。
///
/// **只做形状转换，不重新解析** —— 值、单位码、强转路径都取台账里那一份。
///
/// 转换口径在 [`ItemRow::to_parsed_item`]（与对拍入账共用同一份形状转换，
/// 免得两处各写一份、慢慢漂成两套口径）。
pub(crate) fn item_row_to_parsed_item(row: &ItemRow) -> ParsedItem {
    row.to_parsed_item()
}

/// 对拍差值行 → 对拍结论（回写装配用）。
pub(crate) fn comparison_row_to_verdict(
    row: &ComparisonRow,
    patient_id: Option<&str>,
) -> ComparisonVerdict {
    let state = ComparisonState::lookup(&row.state).unwrap_or(ComparisonState::Skipped);
    let delta = match (row.previous_value, row.current_value, row.unit_code.as_deref()) {
        (Some(previous), Some(current), Some(unit)) => Some(Delta::compute(current, previous, unit)),
        _ => None,
    };
    ComparisonVerdict {
        key: ComparisonKey::new(
            patient_id.unwrap_or_default(),
            row.part_key.clone().unwrap_or_default(),
            row.item_code.clone().unwrap_or_default(),
        ),
        state,
        delta,
        hit_count: usize::from(state == ComparisonState::HasHistory),
        candidates: Vec::new(),
        failure: None,
        anchor: MeasureAnchor::unknown(),
        snapshot_drift: None,
    }
}

/// 从审计上下文取报告主键（留痕与状态推进挂同一份报告）。
fn report_id(ctx: &AuditContext) -> i64 {
    ctx.report_id.unwrap_or_default()
}

/// 回写失败时**释放幂等登记**（否则重试通道再也登记不上这把钥匙）。
fn release_claim(repo: &Repo, idempotency_key: &str) {
    let _ = repo.release_writeback_claim(idempotency_key);
}

/// **定长比较**（不因「早一个字符返回」而泄露令牌长度）。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 从路径里取出检查号（**路径不匹配返回 `None`**）。
///
/// 路径形状：`/api/v1/reports/{study_no}/review`。
pub fn parse_route(path: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    let rest = path.strip_prefix(REVIEW_ROUTE_PREFIX)?;
    let study_no = rest.strip_suffix(REVIEW_ROUTE_SUFFIX)?;
    // 尾缀去掉后不能还带 `/`（那说明多了一层路径）。
    if study_no.is_empty() || study_no.contains('/') {
        return None;
    }
    Some(percent_decode(study_no))
}

/// `%XX` 解码（检查号里偶尔带 URL 编码字符）。
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[idx + 1]), hex_value(bytes[idx + 2])) {
                out.push(high * 16 + low);
                idx += 3;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 十六进制字符的值（不是十六进制返回 `None`）。
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// **内置回执服务端**（最小 HTTP/1.1）。
///
/// 只实现这一个路由 —— 它是**入站回执**，不是通用 Web 服务：
/// 不发目录列表、不渲染页面、不代发静态资源。
pub struct ReviewServer {
    listener: TcpListener,
    handler: Arc<ReviewHandler>,
}

impl ReviewServer {
    /// 绑定监听地址（`127.0.0.1:0` = 让操作系统挑一个空闲端口）。
    pub fn bind(addr: &str, handler: ReviewHandler) -> Result<ReviewServer, ReviewServerError> {
        let listener = TcpListener::bind(addr).map_err(ReviewServerError::Io)?;
        listener
            .set_nonblocking(true)
            .map_err(ReviewServerError::Io)?;
        Ok(ReviewServer {
            listener,
            handler: Arc::new(handler),
        })
    }

    /// 实际监听地址（绑 `127.0.0.1:0` 时用它取到真实端口）。
    pub fn local_addr(&self) -> Result<SocketAddr, ReviewServerError> {
        self.listener.local_addr().map_err(ReviewServerError::Io)
    }

    /// 处理一条连接（一次请求一次应答，**连接即关**）。
    fn serve_connection(&self, mut stream: TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))
            .ok();
        let response = match read_request(&mut stream) {
            Some(request) => self.handler.handle(&request),
            None => ReviewError::new(
                ErrorCode::ReviewBadRequest,
                Some(DetailCode::ReviewBodyNotJson),
                400,
                "请求报文读不出来（没读完 / 超长）",
            )
            .to_response(),
        };
        write_response(&mut stream, &response);
    }

    /// **阻塞式**服务循环（到停机为止）。
    pub fn serve(self) -> Result<(), ReviewServerError> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    self.serve_connection(stream);
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(ReviewServerError::Io(err)),
            }
        }
    }

    /// **后台线程**里服务（守护进程用它）。
    pub fn spawn(self) -> ReviewServerHandle {
        let addr = self.local_addr().ok();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        self.serve_connection(stream);
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        ReviewServerHandle {
            addr,
            stop,
            handle: Some(handle),
        }
    }
}

/// 后台服务端的句柄（**能叫停** —— 守护进程退出时不留半个监听）。
pub struct ReviewServerHandle {
    addr: Option<SocketAddr>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ReviewServerHandle {
    /// 实际监听地址。
    pub fn addr(&self) -> Option<SocketAddr> {
        self.addr
    }

    /// 叫停服务（线程会在下一轮轮询时退出）。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ReviewServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 服务端级错误（绑定失败 / IO 失败）。
#[derive(Debug)]
pub enum ReviewServerError {
    /// IO 错误（绑定失败 / 读写失败）。
    Io(std::io::Error),
    /// 状态库错误。
    Repo(RepoError),
}

impl std::fmt::Display for ReviewServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewServerError::Io(e) => write!(f, "回执端点 IO 错误：{}", e),
            ReviewServerError::Repo(e) => write!(f, "状态库错误：{}", e),
        }
    }
}

impl std::error::Error for ReviewServerError {}

/// 从连接里读出一条完整请求（**读不满就返回 `None`**）。
fn read_request(stream: &mut TcpStream) -> Option<ReviewRequest> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_len, body_start) = loop {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
        if let Some(bounds) = find_header_end(&buffer) {
            break bounds;
        }
        if buffer.len() > MAX_BODY_BYTES {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_len]).to_string();
    let content_length = content_length_of(&head);
    if content_length > MAX_BODY_BYTES {
        return None;
    }
    // 请求体按 `Content-Length` 读满（客户端分片发也要读全）。
    while buffer.len() < body_start + content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }
    let body_end = (body_start + content_length).min(buffer.len());
    let body = String::from_utf8_lossy(&buffer[body_start..body_end]).to_string();
    parse_request(&head, &body)
}

/// 找请求头与请求体的分界（返回 `(头长度, 体起始)`）。
fn find_header_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let text = String::from_utf8_lossy(buffer);
    if let Some(pos) = text.find("\r\n\r\n") {
        return Some((pos, pos + 4));
    }
    if let Some(pos) = text.find("\n\n") {
        return Some((pos, pos + 2));
    }
    None
}

/// 请求头里的 `Content-Length`（没有就是 0）。
fn content_length_of(head: &str) -> usize {
    for line in head.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case("content-length") {
                return value.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

/// 解析请求头与请求体（**头一行是方法 / 路径 / 版本**）。
pub fn parse_request(head: &str, body: &str) -> Option<ReviewRequest> {
    let mut lines = head.lines();
    let request_line = lines.next()?.trim();
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    Some(ReviewRequest {
        method,
        path,
        headers,
        body: body.to_string(),
    })
}

/// 把应答写回连接（**`Connection: close` —— 一次请求一次应答**）。
fn write_response(stream: &mut TcpStream, response: &ReviewResponse) {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let body = response.body.as_bytes();
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一条测试用处理器（状态库指向临时目录；出站客户端为 `None`）。
    fn handler(tag: &str) -> ReviewHandler {
        let dir = std::env::temp_dir().join(format!("urs-review-unit-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        ReviewHandler::new(
            dir.join("state.sqlite"),
            "unit-token".to_string(),
            None,
            None,
        )
    }

    /// 一条合法的确认回执体。
    fn confirm_body() -> String {
        serde_json::json!({
            "decision": "confirm",
            "reviewer": "USR-SONO-EXAMPLE",
            "note": "核对无误"
        })
        .to_string()
    }

    #[test]
    fn 路由只认这一个形状() {
        assert_eq!(
            parse_route("/api/v1/reports/S-001/review"),
            Some("S-001".to_string())
        );
        // 查询串不参与匹配。
        assert_eq!(
            parse_route("/api/v1/reports/S-001/review?from=workstation"),
            Some("S-001".to_string())
        );
        // 多一层 / 少一层 / 空检查号 —— 一律不匹配。
        assert_eq!(parse_route("/api/v1/reports/S-001/review/extra"), None);
        assert_eq!(parse_route("/api/v1/reports//review"), None);
        assert_eq!(parse_route("/api/v1/reports/S-001"), None);
        assert_eq!(parse_route("/api/v1/structured-reports"), None);
        assert_eq!(parse_route("/"), None);
        // URL 编码的检查号解出来。
        assert_eq!(
            parse_route("/api/v1/reports/S%2D001/review"),
            Some("S-001".to_string())
        );
    }

    #[test]
    fn 请求解析出方法路径头与体() {
        let head = "POST /api/v1/reports/S-001/review HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nContent-Length: 2";
        let request = parse_request(head, "{}").expect("应解析出请求");
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/v1/reports/S-001/review");
        assert_eq!(request.header("authorization"), Some("Bearer t"));
        assert_eq!(request.header("CONTENT-LENGTH"), Some("2"));
        assert_eq!(request.body, "{}");
        // 没有请求头分隔符 → 解析不出请求。
        assert!(parse_request("", "").is_none());
    }

    #[test]
    fn 缺令牌与错令牌都被拒() {
        let handler = handler("token");
        let path = "/api/v1/reports/S-001/review";

        // 缺 Authorization 头。
        let response = handler.handle(&ReviewRequest::new("POST", path, None, &confirm_body()));
        assert_eq!(response.status, 401);
        assert_eq!(response.error_code(), Some(ErrorCode::ReviewUnauthorized));
        assert_eq!(response.error_detail(), Some(DetailCode::ReviewTokenMissing));

        // 令牌不对。
        let response = handler.handle(&ReviewRequest::new(
            "POST",
            path,
            Some("wrong-token"),
            &confirm_body(),
        ));
        assert_eq!(response.status, 401);
        assert_eq!(response.error_code(), Some(ErrorCode::ReviewUnauthorized));
        assert_eq!(response.error_detail(), Some(DetailCode::ReviewTokenInvalid));

        // 不是 Bearer 格式。
        let mut request = ReviewRequest::new("POST", path, Some("unit-token"), &confirm_body());
        request.headers.retain(|(key, _)| key != "Authorization");
        request
            .headers
            .push(("Authorization".to_string(), "Token unit-token".to_string()));
        let response = handler.handle(&request);
        assert_eq!(response.status, 401);
        assert_eq!(response.error_detail(), Some(DetailCode::ReviewTokenInvalid));
    }

    #[test]
    fn 路径方法请求体与决定值各自被拒() {
        let handler = handler("route");
        let path = "/api/v1/reports/S-001/review";

        // 路径不匹配。
        let response = handler.handle(&ReviewRequest::new(
            "POST",
            "/api/v1/reports/S-001",
            Some("unit-token"),
            &confirm_body(),
        ));
        assert_eq!(response.status, 404);
        assert_eq!(response.error_code(), Some(ErrorCode::ReviewNotFound));
        assert_eq!(response.error_detail(), Some(DetailCode::ReviewRouteNotFound));

        // 方法不对。
        let response = handler.handle(&ReviewRequest::new(
            "GET",
            path,
            Some("unit-token"),
            &confirm_body(),
        ));
        assert_eq!(response.status, 405);
        assert_eq!(response.error_code(), Some(ErrorCode::ReviewBadRequest));
        assert_eq!(
            response.error_detail(),
            Some(DetailCode::ReviewMethodNotAllowed)
        );

        // 请求体不是 JSON。
        let response = handler.handle(&ReviewRequest::new(
            "POST",
            path,
            Some("unit-token"),
            "not-json",
        ));
        assert_eq!(response.status, 400);
        assert_eq!(response.error_code(), Some(ErrorCode::ReviewBadRequest));
        assert_eq!(response.error_detail(), Some(DetailCode::ReviewBodyNotJson));

        // 决定值不认识 —— 不猜默认动作。
        let body = serde_json::json!({"decision": "maybe", "reviewer": "USR-SONO-EXAMPLE"}).to_string();
        let response = handler.handle(&ReviewRequest::new(
            "POST",
            path,
            Some("unit-token"),
            &body,
        ));
        assert_eq!(response.status, 400);
        assert_eq!(
            response.error_detail(),
            Some(DetailCode::ReviewDecisionInvalid)
        );

        // 缺确认人。
        let body = serde_json::json!({"decision": "confirm"}).to_string();
        let response = handler.handle(&ReviewRequest::new(
            "POST",
            path,
            Some("unit-token"),
            &body,
        ));
        assert_eq!(response.status, 400);
        assert_eq!(
            response.error_detail(),
            Some(DetailCode::ReviewReviewerMissing)
        );
    }

    #[test]
    fn 检查号不存在时显式报找不到() {
        let handler = handler("notfound");
        let response = handler.handle(&ReviewRequest::new(
            "POST",
            "/api/v1/reports/NO-SUCH-STUDY/review",
            Some("unit-token"),
            &confirm_body(),
        ));
        assert_eq!(response.status, 404);
        assert_eq!(response.error_code(), Some(ErrorCode::ReviewNotFound));
        assert_eq!(
            response.error_detail(),
            Some(DetailCode::ReviewReportNotFound)
        );
    }

    #[test]
    fn 失败条目带稳定码与明细且位置不编造() {
        let err = ReviewError::new(
            ErrorCode::ReviewUnauthorized,
            Some(DetailCode::ReviewTokenMissing),
            401,
            "缺 Authorization 头",
        );
        let failure = err.to_failure();
        assert_eq!(failure.code, ErrorCode::ReviewUnauthorized);
        assert_eq!(failure.detail_code(), Some(DetailCode::ReviewTokenMissing));
        // 回执端点的失败不在报告正文里 —— 位置一律 0，不编造行列。
        assert_eq!(failure.line, 0);
        assert_eq!(failure.column, 0);
        assert!(err.message().contains("REVIEW_UNAUTHORIZED"));
    }

    #[test]
    fn 令牌比较是定长的() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn 十六进制解析只认十六进制() {
        assert_eq!(hex_value(b'0'), Some(0));
        assert_eq!(hex_value(b'9'), Some(9));
        assert_eq!(hex_value(b'a'), Some(10));
        assert_eq!(hex_value(b'F'), Some(15));
        assert_eq!(hex_value(b'z'), None);
        assert_eq!(hex_value(b'-'), None);
    }
}

