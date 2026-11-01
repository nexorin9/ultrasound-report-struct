//! 出站：EMR 写回、企业微信推送、医师确认回执端点。
//!
//! 三条出站里的前两条已落地：
//!
//! - [`emr`] —— EMR 结构化报告回写（**改变真实业务状态**的那一步）；
//! - [`wecom`] —— 企业微信群机器人推送（把结果送到人眼前的那一步）；
//!
//! 确认回执端点随 [`review_api`] 落地；**回写通道的重试与降级**随 [`retry`] 落地。
//!
//! **三条出站共用两样东西**：出站共用的 HTTP 小工具（[`read_body`] / [`truncate`]），
//! 以及**共用码表**（`crate::parse::errors`）—— 出站不另造一套错误码，
//! 运维按错误码聚合时，「EMR 写不进去」「企微推不出去」「测值缺单位」在同一张表里。

pub mod emr;
pub mod review_api;
pub mod retry;
pub mod wecom;

pub use emr::{
    assert_no_raw_text, build_payload, forbidden_fragments, idempotency_key,
    scan_forbidden_text, EmrClient, EmrError, EmrErrorKind, EmrWriteReceipt, PayloadBuild,
    StructuredComparison, StructuredItem, StructuredReportPayload, EMR_STRUCTURED_REPORT_PATH,
};
pub use review_api::{
    parse_route, ReviewBody, ReviewError, ReviewHandler, ReviewRequest, ReviewResponse,
    ReviewServer, ReviewServerError, ReviewServerHandle, REVIEW_ROUTE_PREFIX, REVIEW_ROUTE_SUFFIX,
};
pub use retry::{
    finish_writeback, push_writeback_completed, write_with_retry, PushOutcome, RetryError,
    RetryPolicy, WritebackChannel, WritebackResult, DEFAULT_RETRY_BASE_MS, MAX_WRITEBACK_RETRIES,
};
pub use wecom::{
    redact_webhook_url, scan_message_forbidden_text, PushComparison, PushKind, PushSummary,
    WeComClient, WeComError, WeComErrorKind, WeComMessage, WeComReceipt, WeComResponse, WeComText,
    WECOM_MSG_TYPE_TEXT,
};

/// 读取响应体（读不到就是「响应体读不出来」，不是「不是 JSON」—— 两者分开记）。
pub(crate) fn read_body(response: ureq::Response) -> String {
    match response.into_string() {
        Ok(text) => text,
        Err(err) => format!("__URS_BODY_UNREADABLE__{}", err),
    }
}

/// 截断长文本（失败原因进清单，不该带着整个响应体走）。
pub(crate) fn truncate(text: &str) -> String {
    const LIMIT: usize = 200;
    let cleaned = text.trim();
    if cleaned.chars().count() <= LIMIT {
        return cleaned.to_string();
    }
    let head: String = cleaned.chars().take(LIMIT).collect();
    format!("{}…（已截断）", head)
}
