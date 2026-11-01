//! 企业微信群机器人推送客户端 —— **出站推**（本项目三条真实出站之一）。
//!
//! **这条出站是干什么的**：超声医师确认之后，结论写进 EMR，群里也要看到一条 ——
//! 「哪份报告、哪几条差值、哪几条待处理」。这是本产品**把结果送到人眼前**的那一步：
//! 台账躺在 SQLite 里没人看，**推送到群里**才是科室里真的「收到了」。
//!
//! **接口契约**（与 `spec.md` §2.3「出站推送」一行一致）：
//!
//! ```text
//! POST {WECOM_WEBHOOK_URL}
//! Content-Type: application/json
//!
//! {"msgtype":"text","text":{"content":"…"}}
//! ```
//!
//! ## HTTP 200 不等于送达（这一条最容易踩）
//!
//! 企微群机器人的业务错误**不在状态码里，在响应体的 `errcode` 里**：
//!
//! ```text
//! HTTP/1.1 200 OK
//! {"errcode":93000,"errmsg":"invalid webhook url"}
//! ```
//!
//! 只看状态码的客户端会把「被拒收」当成「已送达」——
//! 推送静默失败，群里没人收到，而系统里写着「已推送」。
//! 所以本客户端在状态码之外**必须**再判一次 `errcode`：
//! 非 0 一律判失败（[`classify_errcode`]），**绝不静默成功**。
//!
//! ## 凭据就是地址本身（地址要打码）
//!
//! 群机器人的 key 直接写在 URL 的查询参数里（`?key=…`）——
//! **地址即凭据**。地址一旦进了失败清单 / 审计 / 运行摘要，
//! 就等于把 key 贴到了科务群里。所以**凡是要给人看的地址，一律先打码**
//! （[`redact_webhook_url`]）；实际发请求时才用完整地址。
//!
//! 地址**只从 `WECOM_WEBHOOK_URL` 环境变量读**，配置文件里不落 ——
//! 配置文件常常被人打包发到科室群里。
//!
//! ## 只发结构化摘要（不含原始可识别文本）
//!
//! 推送正文由 [`PushSummary`] 从**结构化字段**装配：检查号、条目数、差值明细
//! （测项码 + 归一值 + 单位码）、待处理条数、词典版本 —— **没有一个字来自报告原文**。
//! 测项码是 ASCII 码（`kidney.ap_diameter`），不是叙述文本。
//! 另有兜底扫描（[`scan_message_forbidden_text`]，复用出站共用的扫描器）：
//! 报告原文里的中文叙述片段一段都不许出现在推送正文里。
//!
//! ## 失败四分类（与出站读写同一套口径）
//!
//! | 语义 | 触发 | 可重试 | 稳定错误码 | 明细归类 |
//! |---|---|---|---|---|
//! | `upstream_unavailable` | 连不上 / 超时 / 5xx / 触发频率限制 | 是 | `WECOM_UPSTREAM_UNAVAILABLE` | `WECOM_TIMEOUT` / `WECOM_CONNECTION_FAILED` / `WECOM_SERVER_ERROR` / `WECOM_ERRCODE_REJECTED` |
//! | `auth_failed` | webhook 地址或 key 无效 | 否 | `WECOM_AUTH_FAILED` | `WECOM_UNAUTHORIZED` |
//! | `bad_response` | 其他 4xx / 响应体不是 JSON / `errcode` 非 0 | 否 | `WECOM_BAD_RESPONSE` | `WECOM_CLIENT_ERROR` / `WECOM_BODY_NOT_JSON` / `WECOM_BODY_SCHEMA_INVALID` / `WECOM_ERRCODE_REJECTED` |
//! | `not_configured` | webhook 地址缺失（还没发请求） | 否 | `WECOM_NOT_CONFIGURED` | — |
//!
//! **退避重试不在这一层**：重试与「重试耗尽转 `pending_writeback` + 告警」
//! 是回写通道的策略，由 `src/outbound/retry.rs` 承担 —— 本层只负责
//! 「发一次、把失败讲清楚」。
//!
//! **错误码不另造一套**：四类语义映射到**全项目共用的那张稳定码表**
//! （[`crate::parse::errors`]）—— 运维按错误码聚合时，
//! 「企微推不出去」和「测值缺单位」在同一张表里，不用记两套规则。

use std::error::Error as StdError;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{Config, Credentials, DEFAULT_REQUEST_TIMEOUT_SECS, ENV_WECOM_WEBHOOK_URL};
use crate::outbound::emr::scan_forbidden_text;
use crate::outbound::{read_body, truncate};
use crate::parse::errors::{DetailCode, ErrorCode, Failure};

/// 企微群机器人的消息类型（本轮只用 `text`）。
pub const WECOM_MSG_TYPE_TEXT: &str = "text";

/// 企微文本消息的内容段（`text` 消息类型下的 `text` 字段）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeComText {
    /// 消息正文。
    pub content: String,
}

/// **企微群机器人消息**（请求体）。
///
/// 契约（`spec.md` §2.3「出站推送」一行）：
///
/// ```json
/// {"msgtype":"text","text":{"content":"…"}}
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeComMessage {
    /// 消息类型（`text`）。
    pub msgtype: String,
    /// 内容段。
    pub text: WeComText,
}

impl WeComMessage {
    /// 造一条文本消息。
    pub fn text(content: &str) -> WeComMessage {
        WeComMessage {
            msgtype: WECOM_MSG_TYPE_TEXT.to_string(),
            text: WeComText {
                content: content.to_string(),
            },
        }
    }

    /// 用摘要装配一条文本消息（**正文只由结构化字段生成**）。
    pub fn from_summary(summary: &PushSummary) -> WeComMessage {
        WeComMessage::text(&summary.render())
    }
}

/// **企微响应体**（成功与失败都在这里，用 `errcode` 区分）。
///
/// ```text
/// 成功：{"errcode":0,"errmsg":"ok"}
/// 失败：{"errcode":93000,"errmsg":"invalid webhook url"}
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeComResponse {
    /// 业务错误码（`0` = 送达）。
    pub errcode: i64,
    /// 企微侧的文字说明。
    #[serde(default)]
    pub errmsg: String,
}

/// 推送摘要的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushKind {
    /// 回写完成（医师已确认，结论已写进 EMR）。
    WritebackCompleted,
    /// 回写失败待人工介入（`pending_writeback` 告警）。
    PendingWritebackAlert,
}

impl PushKind {
    /// 种类字符串（进日志与消息抬头）。
    pub fn as_str(self) -> &'static str {
        match self {
            PushKind::WritebackCompleted => "writeback_completed",
            PushKind::PendingWritebackAlert => "pending_writeback_alert",
        }
    }
}

/// **一条差值**（推送摘要里的明细行）。
///
/// 字段全是**码与数** —— 没有「原始写法」这一位，也没有任何叙述文本。
#[derive(Clone, Debug, PartialEq)]
pub struct PushComparison {
    /// 测项码（如 `kidney.ap_diameter`）。
    pub item_code: String,
    /// 本次归一值。
    pub current_value: f64,
    /// 同侧历史值（归一后）。
    pub previous_value: f64,
    /// 差值 = 本次 − 历史。
    pub delta: f64,
    /// 归一单位码（UCUM）。
    pub unit_code: String,
}

/// **推送摘要**（一眼能核对的：哪份报告、哪几条差值、哪几条待处理）。
///
/// **全部字段都来自结构化结论** —— 没有一个字段能装下报告原文段落。
#[derive(Clone, Debug, PartialEq)]
pub struct PushSummary {
    /// 检查号。
    pub study_no: String,
    /// 摘要种类。
    pub kind: PushKind,
    /// 台账条目数（已落台账的条数）。
    pub item_count: usize,
    /// 差值明细（**只含有历史的那几条** —— 没比成的没有差值可说）。
    pub comparisons: Vec<PushComparison>,
    /// 待处理失败条目数。
    pub pending_failure_count: usize,
    /// 词典版本（口径追溯）。
    pub dictionary_version: String,
    /// 错误码（告警类摘要才有）。
    pub error_code: Option<String>,
    /// **已尝试次数**（告警类摘要才有）—— 运维一眼看得出这是第几次失败。
    pub attempts: Option<usize>,
}

impl PushSummary {
    /// 渲染成推送正文。
    ///
    /// **只写码与数**：测项码、数值、单位码、检查号、条数 ——
    /// 报告里的中文叙述一个字都不进来。
    pub fn render(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        match self.kind {
            PushKind::WritebackCompleted => {
                lines.push("超声报告结构化结论已确认回写".to_string())
            }
            PushKind::PendingWritebackAlert => {
                lines.push("超声报告结构化回写失败，待人工介入".to_string())
            }
        }
        lines.push(format!("检查号：{}", self.study_no));
        lines.push(format!("台账条目：{} 条", self.item_count));
        lines.push(format!("差值条目：{} 条", self.comparisons.len()));
        lines.push(format!("待处理失败条目：{} 条", self.pending_failure_count));
        if let Some(code) = &self.error_code {
            lines.push(format!("错误码：{}", code));
        }
        if let Some(attempts) = self.attempts {
            lines.push(format!("已尝试：{} 次", attempts));
        }
        if !self.dictionary_version.is_empty() {
            lines.push(format!("词典版本：{}", self.dictionary_version));
        }
        if !self.comparisons.is_empty() {
            lines.push("差值明细：".to_string());
            for comparison in &self.comparisons {
                lines.push(format!(
                    "- {} {}{}（上次 {}{}，{} {}{}）",
                    comparison.item_code,
                    format_value(comparison.current_value),
                    comparison.unit_code,
                    format_value(comparison.previous_value),
                    comparison.unit_code,
                    direction_zh(comparison.delta),
                    format_value(comparison.delta.abs()),
                    comparison.unit_code
                ));
            }
        }
        lines.join("\n")
    }
}

/// 差值方向的中文说法（固定三个词，不自由发挥）。
fn direction_zh(delta: f64) -> &'static str {
    if delta > 0.0 {
        "增大"
    } else if delta < 0.0 {
        "减小"
    } else {
        "持平"
    }
}

/// 数值格式化（去掉多余的零；非有限值给个占位符，不编造数字）。
fn format_value(value: f64) -> String {
    if !value.is_finite() {
        return "—".to_string();
    }
    let rounded = (value * 1000.0).round() / 1000.0;
    let text = format!("{:.3}", rounded);
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 出站推失败的四分类语义。
///
/// **只有这四类** —— 推送失败再多也就这几种，留自由文本分支就没法按码聚合。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WeComErrorKind {
    /// 上游不可达：连接被拒 / 超时 / 5xx / 触发频率限制。**可退避重试。**
    UpstreamUnavailable,
    /// 鉴权失败：webhook 地址或 key 无效。重试没意义，要人去换地址。
    AuthFailed,
    /// 响应不合契约：其他 4xx / 响应体不是 JSON / `errcode` 非 0。
    BadResponse,
    /// 地址未配置：还没发请求就失败了。
    NotConfigured,
}

impl WeComErrorKind {
    /// 语义字符串（进日志与失败原因，便于按语义聚合）。
    pub fn as_str(self) -> &'static str {
        match self {
            WeComErrorKind::UpstreamUnavailable => "upstream_unavailable",
            WeComErrorKind::AuthFailed => "auth_failed",
            WeComErrorKind::BadResponse => "bad_response",
            WeComErrorKind::NotConfigured => "not_configured",
        }
    }

    /// 语义 → **全项目共用的**稳定错误码（出站推不另造一套码）。
    pub fn error_code(self) -> ErrorCode {
        match self {
            WeComErrorKind::UpstreamUnavailable => ErrorCode::WecomUpstreamUnavailable,
            WeComErrorKind::AuthFailed => ErrorCode::WecomAuthFailed,
            WeComErrorKind::BadResponse => ErrorCode::WecomBadResponse,
            WeComErrorKind::NotConfigured => ErrorCode::WecomNotConfigured,
        }
    }

    /// 是否值得退避重试。
    ///
    /// 只有「上游不可达」值得 —— 地址错了重试也不会变好，
    /// 报文被拒重试只会再错一次。
    pub fn retryable(self) -> bool {
        matches!(self, WeComErrorKind::UpstreamUnavailable)
    }

    /// 全部语义（穷举清单，便于测试逐条覆盖）。
    pub const ALL: [WeComErrorKind; 4] = [
        WeComErrorKind::UpstreamUnavailable,
        WeComErrorKind::AuthFailed,
        WeComErrorKind::BadResponse,
        WeComErrorKind::NotConfigured,
    ];
}

/// 出站推失败。
#[derive(Clone, Debug, PartialEq)]
pub struct WeComError {
    /// 四分类语义。
    pub kind: WeComErrorKind,
    /// 明细归类（同一语义下的具体坏法；`not_configured` 没有明细）。
    pub detail: Option<DetailCode>,
    /// 中文原因（进失败清单与运维摘要）。
    pub reason: String,
    /// 已尝试次数（1 = 只试过一次就没再试）。
    pub attempts: usize,
}

impl WeComError {
    /// 造一条出站推失败。
    pub fn new(
        kind: WeComErrorKind,
        detail: Option<DetailCode>,
        reason: impl Into<String>,
    ) -> WeComError {
        WeComError {
            kind,
            detail,
            reason: reason.into(),
            attempts: 1,
        }
    }

    /// 未配置（webhook 地址缺失）。
    pub fn not_configured(reason: impl Into<String>) -> WeComError {
        WeComError::new(WeComErrorKind::NotConfigured, None, reason)
    }

    /// 稳定错误码（取自共用码表）。
    pub fn error_code(&self) -> ErrorCode {
        self.kind.error_code()
    }

    /// 错误码字符串。
    pub fn code(&self) -> &'static str {
        self.error_code().as_str()
    }

    /// 是否可重试。
    pub fn retryable(&self) -> bool {
        self.kind.retryable()
    }

    /// 记上已尝试次数。
    pub fn with_attempts(mut self, attempts: usize) -> WeComError {
        self.attempts = attempts;
        self
    }

    /// 面向人的说明：稳定码 + 中文文案 + 细分归类 + 原因。
    pub fn message(&self) -> String {
        let mut text = format!("错误码={} 说明={}", self.code(), self.error_code().message());
        if let Some(detail) = self.detail {
            text.push_str(&format!("（{}）", detail.message()));
        }
        text.push_str(&format!(" 原因={}", self.reason));
        text
    }

    /// 转成**共用**失败条目（进失败清单 / 审计）。
    ///
    /// 位置一律 0 —— 出站失败不在报告正文里，**不编造行列**。
    pub fn to_failure(&self) -> Failure {
        let mut failure = Failure::new(self.error_code(), 0, 0, 0, &self.reason);
        if let Some(detail) = self.detail {
            failure = failure.with_detail_code(detail);
        }
        failure
    }
}

impl std::fmt::Display for WeComError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl StdError for WeComError {}

/// **推送回执**（送达之后的凭据）。
#[derive(Clone, Debug, PartialEq)]
pub struct WeComReceipt {
    /// HTTP 状态码。
    pub status: u16,
    /// 企微业务错误码（`0` = 送达）。
    pub errcode: i64,
    /// 企微侧的文字说明。
    pub errmsg: String,
    /// 响应体片段（截断，供运维排查）。
    pub body_snippet: String,
}

/// 企业微信群机器人推送客户端。
///
/// **可克隆、可跨线程** —— 守护进程里多个 worker 共用一个客户端，
/// 连接池（ureq 的 agent）由它内部管着。
#[derive(Clone, Debug)]
pub struct WeComClient {
    agent: ureq::Agent,
    webhook_url: String,
    timeout: Duration,
}

impl WeComClient {
    /// 用给定 webhook 地址建客户端。
    ///
    /// **地址缺失 → 显式失败**（`WECOM_NOT_CONFIGURED`），
    /// 不静默用一个默认地址糊弄过去 —— 那样运维会以为消息真的发出去了。
    pub fn new(webhook_url: &str, timeout: Duration) -> Result<WeComClient, WeComError> {
        let url = webhook_url.trim();
        if url.is_empty() {
            return Err(WeComError::not_configured(format!(
                "{} 未设置（出站推送地址缺失）",
                ENV_WECOM_WEBHOOK_URL
            )));
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(WeComError::not_configured(format!(
                "{} 不是 http(s) 地址：{}",
                ENV_WECOM_WEBHOOK_URL,
                redact_webhook_url(url)
            )));
        }
        let agent = ureq::AgentBuilder::new().timeout(timeout).build();
        Ok(WeComClient {
            agent,
            webhook_url: url.to_string(),
            timeout,
        })
    }

    /// 从**进程环境变量**建客户端（地址 `WECOM_WEBHOOK_URL`）。
    ///
    /// 地址**只走环境变量** —— 配置文件里不落（地址里带着 key）。
    pub fn from_env() -> Result<WeComClient, WeComError> {
        let url = std::env::var(ENV_WECOM_WEBHOOK_URL).unwrap_or_default();
        WeComClient::new(&url, Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS))
    }

    /// 从配置 + 凭据建客户端（超时取配置，**地址只取环境变量**）。
    pub fn from_config(config: &Config, creds: &Credentials) -> Result<WeComClient, WeComError> {
        let url = creds.wecom_webhook_url.clone().unwrap_or_default();
        WeComClient::new(&url, Duration::from_secs(config.request_timeout_secs))
    }

    /// 完整 webhook 地址（**含 key**）—— 只用于发请求。
    ///
    /// 别拿它进日志 / 错误说明 / 失败清单 —— 用 [`WeComClient::display_endpoint`]。
    pub fn endpoint(&self) -> String {
        self.webhook_url.clone()
    }

    /// **打码后**的 webhook 地址 —— 给人看的唯一形态。
    pub fn display_endpoint(&self) -> String {
        redact_webhook_url(&self.webhook_url)
    }

    /// 请求超时上限。
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// **推送一次**（单次尝试；退避重试由回写通道承担）。
    ///
    /// 请求头只带 `Content-Type` 与 `Accept` ——
    /// 群机器人的 key 在地址里，不需要（也不该）另带一个令牌头。
    pub fn send(&self, message: &WeComMessage) -> Result<WeComReceipt, WeComError> {
        // 请求本身先过关：空消息发出去只会在群里留一条空白。
        if message.text.content.trim().is_empty() {
            return Err(WeComError::new(
                WeComErrorKind::BadResponse,
                Some(DetailCode::WecomRequestInvalid),
                "推送内容为空，不发空消息",
            ));
        }
        if message.msgtype.trim().is_empty() {
            return Err(WeComError::new(
                WeComErrorKind::BadResponse,
                Some(DetailCode::WecomRequestInvalid),
                "消息类型为空，不发无类型消息",
            ));
        }

        let body = serde_json::to_string(message).map_err(|err| {
            WeComError::new(
                WeComErrorKind::BadResponse,
                Some(DetailCode::WecomRequestInvalid),
                format!("请求体序列化失败：{}", err),
            )
        })?;

        let started = Instant::now();
        let response = match self
            .agent
            .post(&self.endpoint())
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .send_string(&body)
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(status, resp)) => {
                let body = read_body(resp);
                return Err(classify_status(status, &body));
            }
            Err(ureq::Error::Transport(transport)) => {
                return Err(classify_transport(
                    &transport,
                    started.elapsed(),
                    self.timeout,
                ));
            }
        };

        let status = response.status();
        let text = read_body(response);
        // **HTTP 200 不等于送达** —— 企微把业务错误放在响应体的 errcode 里。
        // 这里必须再判一次，非 0 一律判失败（绝不静默成功）。
        let parsed = parse_response(&text)?;
        if parsed.errcode != 0 {
            return Err(classify_errcode(parsed.errcode, &parsed.errmsg, &text));
        }
        Ok(WeComReceipt {
            status,
            errcode: parsed.errcode,
            errmsg: parsed.errmsg,
            body_snippet: snippet_of(&text),
        })
    }

    /// 按摘要推送一条（**正文由结构化字段生成**）。
    pub fn push(&self, summary: &PushSummary) -> Result<WeComReceipt, WeComError> {
        self.send(&WeComMessage::from_summary(summary))
    }
}

/// 解析企微响应体（**不合契约就判失败，不猜**）。
///
/// 三种坏法各有各的明细归类：不是 JSON / 不是 JSON 对象 / 缺 `errcode` 字段。
fn parse_response(body: &str) -> Result<WeComResponse, WeComError> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|err| {
        WeComError::new(
            WeComErrorKind::BadResponse,
            Some(DetailCode::WecomBodyNotJson),
            format!(
                "企微响应体不是 JSON（HTTP 200 也不代表送达）：{}：{}",
                err,
                snippet_of(body)
            ),
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        WeComError::new(
            WeComErrorKind::BadResponse,
            Some(DetailCode::WecomBodyNotJson),
            format!("企微响应体不是 JSON 对象：{}", snippet_of(body)),
        )
    })?;
    let errcode = object
        .get("errcode")
        .and_then(|raw| raw.as_i64())
        .ok_or_else(|| {
            WeComError::new(
                WeComErrorKind::BadResponse,
                Some(DetailCode::WecomBodySchemaInvalid),
                format!("企微响应体缺 errcode 字段，不符合接口契约：{}", snippet_of(body)),
            )
        })?;
    let errmsg = object
        .get("errmsg")
        .and_then(|raw| raw.as_str())
        .unwrap_or("")
        .to_string();
    Ok(WeComResponse { errcode, errmsg })
}

/// 按 HTTP 状态码判定错误语义（**纯函数**，便于逐条断言）。
pub fn classify_status(status: u16, body: &str) -> WeComError {
    let snippet = snippet_of(body);
    match status {
        401 | 403 => WeComError::new(
            WeComErrorKind::AuthFailed,
            Some(DetailCode::WecomUnauthorized),
            format!("企微返回 {}（webhook 地址或 key 无效）：{}", status, snippet),
        ),
        s if s >= 500 => WeComError::new(
            WeComErrorKind::UpstreamUnavailable,
            Some(DetailCode::WecomServerError),
            format!("企微返回 {}（上游服务自身出错）：{}", s, snippet),
        ),
        s if s >= 400 => WeComError::new(
            WeComErrorKind::BadResponse,
            Some(DetailCode::WecomClientError),
            format!("企微返回 {}（请求被拒）：{}", s, snippet),
        ),
        s => WeComError::new(
            WeComErrorKind::BadResponse,
            Some(DetailCode::WecomClientError),
            format!("企微返回非预期的状态码 {}：{}", s, snippet),
        ),
    }
}

/// **按企微业务错误码判定错误语义**（HTTP 200 也要判 —— 这一步最容易漏）。
///
/// | `errcode` | 含义 | 语义 | 可重试 |
/// |---|---|---|---|
/// | `93000` / `93004` | webhook 地址或 key 无效 | `auth_failed` | 否（要换地址） |
/// | `45009` | 触发接口调用频率限制 | `upstream_unavailable` | 是（退避后再试） |
/// | 其余非 0 | 企微侧拒收 | `bad_response` | 否 |
pub fn classify_errcode(errcode: i64, errmsg: &str, body: &str) -> WeComError {
    let snippet = snippet_of(body);
    match errcode {
        93000 | 93004 => WeComError::new(
            WeComErrorKind::AuthFailed,
            Some(DetailCode::WecomUnauthorized),
            format!(
                "企微拒收（errcode={}，webhook 地址或 key 无效，换地址才能好）：{}：{}",
                errcode, errmsg, snippet
            ),
        ),
        45009 => WeComError::new(
            WeComErrorKind::UpstreamUnavailable,
            Some(DetailCode::WecomErrcodeRejected),
            format!(
                "企微触发频率限制（errcode={}），退避后可再试：{}",
                errcode, errmsg
            ),
        ),
        _ => WeComError::new(
            WeComErrorKind::BadResponse,
            Some(DetailCode::WecomErrcodeRejected),
            format!("企微拒收（errcode={}）：{}：{}", errcode, errmsg, snippet),
        ),
    }
}

/// 按传输层错误判定错误语义（**纯函数**，便于逐条断言）。
///
/// **超时优先于「连不上」**：超时与连接失败在运维眼里是两回事 ——
/// 超时通常是企微侧处理慢，连不上通常是网络或群机器人地址写错。
pub fn classify_transport(
    transport: &ureq::Transport,
    elapsed: Duration,
    timeout: Duration,
) -> WeComError {
    let kind = transport.kind();
    let message = transport.message().unwrap_or("");
    let timed_out = transport
        .source()
        .and_then(|src| src.downcast_ref::<std::io::Error>())
        .map(|err| err.kind() == std::io::ErrorKind::TimedOut)
        .unwrap_or(false)
        // 兜底：耗时顶到超时上限（ureq 内部实现变了也判得出来）。
        || elapsed >= timeout;
    let reason = format!(
        "企微请求未完成（{:?}，耗时 {} 毫秒，上限 {} 毫秒）：{}",
        kind,
        elapsed.as_millis(),
        timeout.as_millis(),
        snippet_of(message)
    );
    match kind {
        ureq::ErrorKind::BadStatus | ureq::ErrorKind::BadHeader => WeComError::new(
            WeComErrorKind::BadResponse,
            Some(DetailCode::WecomClientError),
            format!("企微响应报文不合 HTTP 规范：{}", reason),
        ),
        ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => WeComError::new(
            WeComErrorKind::BadResponse,
            Some(DetailCode::WecomClientError),
            format!("企微地址不合法：{}", reason),
        ),
        _ if timed_out => WeComError::new(
            WeComErrorKind::UpstreamUnavailable,
            Some(DetailCode::WecomTimeout),
            reason,
        ),
        _ => WeComError::new(
            WeComErrorKind::UpstreamUnavailable,
            Some(DetailCode::WecomConnectionFailed),
            reason,
        ),
    }
}

/// **把文本里出现的密钥形状参数打码** —— 凭据就是地址本身。
///
/// 群机器人的 key 直接写在地址的查询参数里（`?key=…`）。地址一旦进了失败清单 /
/// 审计 / 运行摘要，就等于把 key 贴到了科务群里。
///
/// **不只打码地址**：网关返回的 HTML 错误页**会把请求地址原样回显**，
/// 里面就带着 `key=…`。所以凡是**要离开本进程**的文本（失败原因、响应体片段、
/// 日志行），都要先过这一道 —— 只打码地址是不够的。
///
/// 判据（两条同时成立才算密钥参数）：
///
/// 1. 参数名是**密钥形状**的（`key` / `token` / `secret` / `sign` 等）；
/// 2. 参数名前面是**分隔符**（`?` `&` 引号 空白 `<` 等）或文本开头 ——
///    这样 `monkey=…` 不会被误当成 `key`。
pub fn scrub_secret_params(text: &str) -> String {
    /// 会被打码的参数名（小写比较）。
    const SECRET_PARAMS: [&str; 7] = [
        "key",
        "token",
        "secret",
        "access_token",
        "sign",
        "signature",
        "webhook_key",
    ];

    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        let preceded_ok = i == 0
            || matches!(
                chars[i - 1],
                '&' | '?' | '"' | '\'' | ' ' | '\t' | '\n' | '\r' | '<' | '('
            );
        if preceded_ok {
            let tail: String = chars[i..].iter().collect::<String>().to_ascii_lowercase();
            let matched = SECRET_PARAMS
                .iter()
                .copied()
                .find(|name| tail.starts_with(name) && tail[name.len()..].starts_with('='));
            if let Some(name) = matched {
                out.push_str(&chars[i..i + name.len()].iter().collect::<String>());
                out.push('=');
                i += name.len() + 1;
                // 值：一直吃到分隔符为止。
                let mut has_value = false;
                while i < chars.len()
                    && !matches!(
                        chars[i],
                        '&' | '"' | '\'' | '<' | '>' | ')' | ';' | ' ' | '\t' | '\n' | '\r'
                    )
                {
                    has_value = true;
                    i += 1;
                }
                if has_value {
                    out.push_str("***");
                }
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// **把 webhook 地址里的 key 打码** —— 给人和日志看的唯一形态。
///
/// 主机名与路径保留 —— 运维要靠它们分辨是哪个群机器人；
/// 值一律换成 `***`。
pub fn redact_webhook_url(url: &str) -> String {
    scrub_secret_params(url)
}

/// 失败原因里的响应体片段（**先打码再截断** —— 网关错误页会回显请求地址）。
fn snippet_of(body: &str) -> String {
    truncate(&scrub_secret_params(body))
}

/// **扫描推送正文里有没有混进原文叙述片段**（复用出站共用的扫描器）。
///
/// 返回命中的片段（**空 = 干净**）。与 EMR 出站写用同一把尺子 ——
/// 「外发不带原文」这条纪律在三条出站上是同一条。
pub fn scan_message_forbidden_text(message: &WeComMessage, raw_text: &str) -> Vec<String> {
    scan_forbidden_text(&message.text.content, raw_text)
}

/// 断言推送正文不含原文叙述片段（命中即报出是哪几段）。
pub fn assert_no_raw_text(message: &WeComMessage, raw_text: &str) -> Result<(), String> {
    let hits = scan_message_forbidden_text(message, raw_text);
    if hits.is_empty() {
        Ok(())
    } else {
        Err(format!("推送正文里出现了原始报告叙述片段：{}", hits.join("；")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一份带叙述段的脱敏报告（叙述段是**不允许**外发的东西）。
    const REPORT: &str = "超声所见：\n左肾前后径 3.2cm\n肾盂轻度扩张\n提示：左肾积水，请结合临床。";

    /// 一条典型的「回写完成」摘要。
    fn summary() -> PushSummary {
        PushSummary {
            study_no: "SAMPLE-URO-001".to_string(),
            kind: PushKind::WritebackCompleted,
            item_count: 3,
            comparisons: vec![PushComparison {
                item_code: "kidney.ap_diameter".to_string(),
                current_value: 32.0,
                previous_value: 28.0,
                delta: 4.0,
                unit_code: "mm".to_string(),
            }],
            pending_failure_count: 2,
            dictionary_version: "parts-0.2.0+items-0.1.0".to_string(),
            error_code: None,
            attempts: None,
        }
    }

    #[test]
    fn 消息体结构合契约() {
        let message = WeComMessage::text("你好");
        let json = serde_json::to_string(&message).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(keys, vec!["msgtype", "text"]);
        assert_eq!(value["msgtype"], "text");
        assert_eq!(value["text"]["content"], "你好");
    }

    #[test]
    fn 摘要渲染出可核对的四要素() {
        let text = summary().render();
        assert!(text.contains("检查号：SAMPLE-URO-001"));
        assert!(text.contains("台账条目：3 条"));
        assert!(text.contains("差值条目：1 条"));
        assert!(text.contains("待处理失败条目：2 条"));
        assert!(text.contains("kidney.ap_diameter"));
        assert!(text.contains("32mm"));
        assert!(text.contains("28mm"));
        assert!(text.contains("增大"));
    }

    #[test]
    fn 推送正文不含原文叙述片段() {
        let message = WeComMessage::from_summary(&summary());
        let hits = scan_message_forbidden_text(&message, REPORT);
        assert!(hits.is_empty(), "推送正文里混进了原文叙述：{:?}", hits);
        assert!(assert_no_raw_text(&message, REPORT).is_ok());
    }

    #[test]
    fn 地址缺失判未配置() {
        let err = WeComClient::new("", Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind, WeComErrorKind::NotConfigured);
        assert_eq!(err.code(), "WECOM_NOT_CONFIGURED");
        assert!(err.reason.contains("WECOM_WEBHOOK_URL"));

        let err = WeComClient::new("  ", Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind, WeComErrorKind::NotConfigured);

        let err = WeComClient::new("qyapi.weixin.qq.com/x", Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind, WeComErrorKind::NotConfigured);
    }

    #[test]
    fn 错误码四类语义映射到共用码表() {
        for kind in WeComErrorKind::ALL {
            let code = kind.error_code();
            assert_eq!(ErrorCode::lookup(code.as_str()), Some(code));
        }
        assert_eq!(
            WeComErrorKind::UpstreamUnavailable.error_code(),
            ErrorCode::WecomUpstreamUnavailable
        );
        assert_eq!(
            WeComErrorKind::AuthFailed.error_code(),
            ErrorCode::WecomAuthFailed
        );
        assert_eq!(
            WeComErrorKind::BadResponse.error_code(),
            ErrorCode::WecomBadResponse
        );
        assert_eq!(
            WeComErrorKind::NotConfigured.error_code(),
            ErrorCode::WecomNotConfigured
        );
        assert!(WeComErrorKind::UpstreamUnavailable.retryable());
        assert!(!WeComErrorKind::AuthFailed.retryable());
        assert!(!WeComErrorKind::BadResponse.retryable());
        assert!(!WeComErrorKind::NotConfigured.retryable());
    }

    #[test]
    fn 地址里的key被打码() {
        let redacted = redact_webhook_url("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abcdef-123456");
        assert!(!redacted.contains("abcdef-123456"), "key 泄出来了：{}", redacted);
        assert!(redacted.contains("key=***"));
        // 主机名与路径保留 —— 运维要靠它分辨是哪个群机器人。
        assert!(redacted.contains("qyapi.weixin.qq.com"));
        assert!(redacted.contains("/cgi-bin/webhook/send"));
        // 非密钥参数不动。
        let other = redact_webhook_url("https://example.internal/hook?foo=bar&key=zzz");
        assert!(other.contains("foo=bar"));
        assert!(!other.contains("zzz"));
        // 没有查询参数的地址原样返回。
        assert_eq!(
            redact_webhook_url("https://example.internal/hook"),
            "https://example.internal/hook"
        );
    }

    #[test]
    fn 错误说明里不出现完整地址() {
        let client = WeComClient::new(
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=secret-key-value",
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(!client.display_endpoint().contains("secret-key-value"));
        assert!(client.endpoint().contains("secret-key-value"));
    }

    #[test]
    fn 空内容不发请求() {
        let client =
            WeComClient::new("https://example.internal/hook", Duration::from_secs(1)).unwrap();
        let err = client.send(&WeComMessage::text("   ")).unwrap_err();
        assert_eq!(err.kind, WeComErrorKind::BadResponse);
        assert_eq!(err.detail, Some(DetailCode::WecomRequestInvalid));
    }

    #[test]
    fn 状态码分类逐条对得上() {
        for status in [401u16, 403] {
            let err = classify_status(status, "denied");
            assert_eq!(err.kind, WeComErrorKind::AuthFailed);
            assert_eq!(err.detail, Some(DetailCode::WecomUnauthorized));
            assert!(!err.retryable());
        }
        for status in [500u16, 502, 503, 504] {
            let err = classify_status(status, "boom");
            assert_eq!(err.kind, WeComErrorKind::UpstreamUnavailable);
            assert_eq!(err.detail, Some(DetailCode::WecomServerError));
            assert!(err.retryable());
        }
        for status in [400u16, 404, 422] {
            let err = classify_status(status, "nope");
            assert_eq!(err.kind, WeComErrorKind::BadResponse);
            assert_eq!(err.detail, Some(DetailCode::WecomClientError));
            assert!(!err.retryable());
        }
    }

    #[test]
    fn 业务错误码分类逐条对得上() {
        // 地址 / key 无效 → 鉴权失败（重试无用，要换地址）。
        let err = classify_errcode(93000, "invalid webhook url", "{}");
        assert_eq!(err.kind, WeComErrorKind::AuthFailed);
        assert_eq!(err.detail, Some(DetailCode::WecomUnauthorized));
        assert!(!err.retryable());

        // 频率限制 → 上游不可达（退避后可再试）。
        let err = classify_errcode(45009, "api freq out of limit", "{}");
        assert_eq!(err.kind, WeComErrorKind::UpstreamUnavailable);
        assert_eq!(err.detail, Some(DetailCode::WecomErrcodeRejected));
        assert!(err.retryable());

        // 其余非 0 → 拒收。
        let err = classify_errcode(40001, "invalid credential", "{}");
        assert_eq!(err.kind, WeComErrorKind::BadResponse);
        assert_eq!(err.detail, Some(DetailCode::WecomErrcodeRejected));
        assert!(!err.retryable());
    }

    #[test]
    fn 响应体不合契约一律判失败() {
        // 不是 JSON。
        let err = parse_response("<html>502</html>").unwrap_err();
        assert_eq!(err.kind, WeComErrorKind::BadResponse);
        assert_eq!(err.detail, Some(DetailCode::WecomBodyNotJson));

        // 是 JSON 但不是对象。
        let err = parse_response("[1,2,3]").unwrap_err();
        assert_eq!(err.detail, Some(DetailCode::WecomBodyNotJson));

        // 是对象但缺 errcode。
        let err = parse_response("{\"errmsg\":\"ok\"}").unwrap_err();
        assert_eq!(err.detail, Some(DetailCode::WecomBodySchemaInvalid));

        // 正常响应。
        let parsed = parse_response("{\"errcode\":0,\"errmsg\":\"ok\"}").unwrap();
        assert_eq!(parsed.errcode, 0);
        assert_eq!(parsed.errmsg, "ok");
    }

    #[test]
    fn 失败条目转共用码表且不带编造位置() {
        let err = classify_status(503, "boom");
        let failure = err.to_failure();
        assert_eq!(failure.code, ErrorCode::WecomUpstreamUnavailable);
        assert_eq!(failure.line, 0);
        assert_eq!(failure.column, 0);
        assert_eq!(failure.detail_code(), Some(DetailCode::WecomServerError));
    }
}
