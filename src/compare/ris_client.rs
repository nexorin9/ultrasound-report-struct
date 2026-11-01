//! RIS 历史取数客户端 —— **出站读**（本项目三条真实出站之一）。
//!
//! **这条出站是干什么的**：超声医师签报告前要看「这个患者**同侧**部位上次是多少」。
//! 那些历史值在院内 RIS 里，本产品**不攒第二份** —— 每次对拍都回 RIS 取，
//! 取回来的是**权威值**，不是本产品自己留的副本（副本会过期，会跟 RIS 打架）。
//!
//! **接口契约**（与 `spec.md` §2.3 一致）：
//!
//! ```text
//! GET {RIS_BASE_URL}/api/v1/ultrasound-reports?patient_id=&body_part=&since=
//! Authorization: Bearer $RIS_API_TOKEN
//! ```
//!
//! 响应字段：`{study_no, patient_id, body_part, performed_at, measurements:[{item_code,value,unit}]}`。
//! **顶层既接受数组也接受单个对象**（对象按「只有一份历史报告」归一成单元素数组）——
//! 院内 RIS 的这一版接口两种形状都见过，客户端不替 RIS 做假设。
//!
//! **凭据只从环境变量读**（`RIS_API_TOKEN`）：配置文件里出现密钥形状的键**启动即拒**
//! （见 `config.rs`）—— 配置文件常常被人打包发到科室群里，密钥一旦进去等于泄漏。
//!
//! **四分类错误语义**（出站读的失败只有这四种，不留自由文本分支）：
//!
//! | 语义 | 触发 | 可重试 | 稳定错误码 | 明细归类 |
//! |---|---|---|---|---|
//! | `upstream_unavailable` | 连不上 / 超时 / 5xx | 是 | `RIS_UPSTREAM_UNAVAILABLE` | `RIS_TIMEOUT` / `RIS_CONNECTION_FAILED` / `RIS_SERVER_ERROR` |
//! | `auth_failed` | 401 / 403 | 否 | `RIS_AUTH_FAILED` | `RIS_UNAUTHORIZED` |
//! | `bad_response` | 非 JSON / 字段不合契约 / 其他 4xx | 否 | `RIS_BAD_RESPONSE` | `RIS_BODY_NOT_JSON` / `RIS_BODY_SCHEMA_INVALID` / `RIS_BODY_UNREADABLE` / `RIS_CLIENT_ERROR` |
//! | `not_configured` | 端点或凭据缺失（还没发请求） | 否 | `RIS_NOT_CONFIGURED` | — |
//!
//! **重试只给「上游不可达」**：退避重试 ≤ 2 次（最多 3 次尝试）。
//! 鉴权失败重试没意义（令牌不会自己变好），响应不合契约重试也只会再错一次。
//!
//! **失败绝不静默**：取不到历史值 → [`RisError`]（带稳定码 + 明细 + 原因），
//! 由调用方落 `comparison_skipped: upstream_unavailable` ——
//! **绝不当作「无差异」**：那会让医师以为「跟上次一样」，而其实压根没比成。
//!
//! **错误码不另造一套**：四类语义映射到**全项目共用的那张稳定码表**
//! （[`crate::parse::errors`]）—— 运维按错误码聚合时，
//! 「RIS 连不上」和「测值缺单位」在同一张表里，不用记两套规则。

use std::error::Error as StdError;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::{Config, Credentials, DEFAULT_REQUEST_TIMEOUT_SECS, ENV_RIS_API_TOKEN, ENV_RIS_BASE_URL};
use crate::parse::errors::{DetailCode, ErrorCode, Failure};

/// RIS 历史报告接口路径（相对 `RIS_BASE_URL`）。
pub const RIS_HISTORY_PATH: &str = "/api/v1/ultrasound-reports";

/// 退避重试次数上限（**≤ 2**，与 `spec.md` §4「退避重试 ≤2 次」一致）。
pub const DEFAULT_MAX_RETRIES: usize = 2;

/// 退避基数（第 n 次重试等 `base * n` 毫秒）。
pub const DEFAULT_BACKOFF_MS: u64 = 200;

/// 出站读失败的四分类语义。
///
/// **只有这四类** —— 出站读的失败再多也就这几种，留自由文本分支就没法按码聚合。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RisErrorKind {
    /// 上游不可达：连接被拒 / 超时 / 5xx。**可退避重试。**
    UpstreamUnavailable,
    /// 鉴权失败：401 / 403。重试没意义，要人去换令牌。
    AuthFailed,
    /// 响应不合契约：非 JSON / 字段缺失 / 其他 4xx。
    BadResponse,
    /// 端点或凭据未配置：还没发请求就失败了。
    NotConfigured,
}

impl RisErrorKind {
    /// 语义字符串（进日志与失败原因，便于按语义聚合）。
    pub fn as_str(self) -> &'static str {
        match self {
            RisErrorKind::UpstreamUnavailable => "upstream_unavailable",
            RisErrorKind::AuthFailed => "auth_failed",
            RisErrorKind::BadResponse => "bad_response",
            RisErrorKind::NotConfigured => "not_configured",
        }
    }

    /// 语义 → **全项目共用的**稳定错误码（对拍层不另造一套码）。
    pub fn error_code(self) -> ErrorCode {
        match self {
            RisErrorKind::UpstreamUnavailable => ErrorCode::RisUpstreamUnavailable,
            RisErrorKind::AuthFailed => ErrorCode::RisAuthFailed,
            RisErrorKind::BadResponse => ErrorCode::RisBadResponse,
            RisErrorKind::NotConfigured => ErrorCode::RisNotConfigured,
        }
    }

    /// 是否值得退避重试。
    ///
    /// 只有「上游不可达」值得 —— 鉴权失败重试令牌也不会变好，
    /// 响应不合契约重试只会再错一次。
    pub fn retryable(self) -> bool {
        matches!(self, RisErrorKind::UpstreamUnavailable)
    }

    /// 全部语义（穷举清单，便于测试逐条覆盖）。
    pub const ALL: [RisErrorKind; 4] = [
        RisErrorKind::UpstreamUnavailable,
        RisErrorKind::AuthFailed,
        RisErrorKind::BadResponse,
        RisErrorKind::NotConfigured,
    ];
}

/// 出站读失败。
#[derive(Clone, Debug, PartialEq)]
pub struct RisError {
    /// 四分类语义。
    pub kind: RisErrorKind,
    /// 明细归类（同一语义下的具体坏法；`not_configured` 没有明细）。
    pub detail: Option<DetailCode>,
    /// 中文原因（进失败清单与运维摘要）。
    pub reason: String,
    /// 已尝试次数（1 = 只试过一次就没再试）。
    pub attempts: usize,
}

impl RisError {
    /// 造一条出站读失败。
    pub fn new(kind: RisErrorKind, detail: Option<DetailCode>, reason: impl Into<String>) -> RisError {
        RisError {
            kind,
            detail,
            reason: reason.into(),
            attempts: 1,
        }
    }

    /// 未配置（端点或凭据缺失）。
    pub fn not_configured(reason: impl Into<String>) -> RisError {
        RisError::new(RisErrorKind::NotConfigured, None, reason)
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
    pub fn with_attempts(mut self, attempts: usize) -> RisError {
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

impl std::fmt::Display for RisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl StdError for RisError {}

/// RIS 历史查询条件。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RisQuery {
    /// 患者主索引（**必填** —— 没有它就不知道跟谁比）。
    pub patient_id: String,
    /// 部位过滤（可选；规范部位键或院内写法，由 RIS 侧解释）。
    pub body_part: Option<String>,
    /// 起始时间（可选；由 RIS 侧解释其格式）。
    pub since: Option<String>,
}

impl RisQuery {
    /// 只按患者查。
    pub fn for_patient(patient_id: impl Into<String>) -> RisQuery {
        RisQuery {
            patient_id: patient_id.into(),
            ..RisQuery::default()
        }
    }

    /// 限定部位。
    pub fn with_body_part(mut self, body_part: impl Into<String>) -> RisQuery {
        self.body_part = Some(body_part.into());
        self
    }

    /// 限定起始时间。
    pub fn with_since(mut self, since: impl Into<String>) -> RisQuery {
        self.since = Some(since.into());
        self
    }
}

/// RIS 历史报告里的一条测量值。
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RisReportMeasurement {
    /// 测项码。
    pub item_code: String,
    /// 数值（RIS 侧可能给 JSON 数字，也可能给字符串 —— 两种都收）。
    #[serde(default, deserialize_with = "de_opt_f64")]
    pub value: Option<f64>,
    /// 原单位。
    #[serde(default)]
    pub unit: Option<String>,
}

/// RIS 的一份历史报告。
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RisReport {
    /// 检查号（**必填** —— 没有它认不出这是哪份报告）。
    pub study_no: String,
    /// 患者主索引。
    #[serde(default)]
    pub patient_id: Option<String>,
    /// 部位。
    #[serde(default)]
    pub body_part: Option<String>,
    /// 检查日期时间。
    #[serde(default)]
    pub performed_at: Option<String>,
    /// 测量值列表（**必填** —— 可以是空数组，但字段不能缺）。
    pub measurements: Vec<RisReportMeasurement>,
}

/// RIS 历史取数客户端。
///
/// **可克隆、可跨线程** —— 守护进程里多个 worker 共用一个客户端，
/// 连接池（ureq 的 agent）由它内部管着。
#[derive(Clone, Debug)]
pub struct RisClient {
    agent: ureq::Agent,
    base_url: String,
    token: String,
    timeout: Duration,
    max_retries: usize,
    backoff: Duration,
}

impl RisClient {
    /// 用给定端点与凭据建客户端。
    ///
    /// **端点或凭据缺失 → 显式失败**（`RIS_NOT_CONFIGURED`），
    /// 不静默用一个默认端点糊弄过去 —— 那样运维会以为连的是真 RIS。
    pub fn new(base_url: &str, token: &str, timeout: Duration) -> Result<RisClient, RisError> {
        let base = base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(RisError::not_configured(format!(
                "{} 未设置（出站读端点缺失）",
                ENV_RIS_BASE_URL
            )));
        }
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(RisError::not_configured(format!(
                "{} 不是 http(s) 端点：{}",
                ENV_RIS_BASE_URL, base
            )));
        }
        if token.trim().is_empty() {
            return Err(RisError::not_configured(format!(
                "{} 未设置（出站读凭据缺失）",
                ENV_RIS_API_TOKEN
            )));
        }
        let agent = ureq::AgentBuilder::new().timeout(timeout).build();
        Ok(RisClient {
            agent,
            base_url: base.to_string(),
            token: token.trim().to_string(),
            timeout,
            max_retries: DEFAULT_MAX_RETRIES,
            backoff: Duration::from_millis(DEFAULT_BACKOFF_MS),
        })
    }

    /// 从**进程环境变量**建客户端（端点 `RIS_BASE_URL`、凭据 `RIS_API_TOKEN`）。
    ///
    /// 凭据只走环境变量 —— 配置文件里不落密钥。
    pub fn from_env() -> Result<RisClient, RisError> {
        let base = std::env::var(ENV_RIS_BASE_URL).unwrap_or_default();
        let token = std::env::var(ENV_RIS_API_TOKEN).unwrap_or_default();
        RisClient::new(
            &base,
            &token,
            Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
        )
    }

    /// 从配置 + 凭据建客户端（端点取配置/环境变量，凭据只取环境变量）。
    pub fn from_config(config: &Config, creds: &Credentials) -> Result<RisClient, RisError> {
        let base = config.ris_base_url.clone().unwrap_or_default();
        let token = creds.ris_api_token.clone().unwrap_or_default();
        RisClient::new(
            &base,
            &token,
            Duration::from_secs(config.request_timeout_secs),
        )
    }

    /// 设置退避重试次数（**上限 2 次**，传更大的值会被截断）。
    pub fn with_max_retries(mut self, max_retries: usize) -> RisClient {
        self.max_retries = max_retries.min(DEFAULT_MAX_RETRIES);
        self
    }

    /// 设置退避基数。
    pub fn with_backoff(mut self, backoff: Duration) -> RisClient {
        self.backoff = backoff;
        self
    }

    /// 出站读端点（全路径，不含查询串）。
    pub fn endpoint(&self) -> String {
        format!("{}{}", self.base_url, RIS_HISTORY_PATH)
    }

    /// 请求超时上限。
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// 退避重试次数上限。
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    /// 取该患者的历史超声报告。
    ///
    /// 失败语义见模块头：四分类语义 + 共用稳定码。
    pub fn fetch_history(&self, query: &RisQuery) -> Result<Vec<RisReport>, RisError> {
        if query.patient_id.trim().is_empty() {
            return Err(RisError::new(
                RisErrorKind::BadResponse,
                Some(DetailCode::RisClientError),
                "查询条件缺 patient_id（没有患者主索引就对不了历史值）",
            ));
        }
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match self.attempt(query) {
                Ok(reports) => return Ok(reports),
                Err(err) => {
                    // 只有「上游不可达」值得再试；鉴权失败 / 响应不合契约重试也只会再错一次。
                    if err.retryable() && attempt <= self.max_retries {
                        std::thread::sleep(self.backoff * attempt as u32);
                        continue;
                    }
                    return Err(err.with_attempts(attempt));
                }
            }
        }
    }

    /// 单次尝试（不含退避重试）。
    fn attempt(&self, query: &RisQuery) -> Result<Vec<RisReport>, RisError> {
        let mut req = self
            .agent
            .get(&self.endpoint())
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json");
        req = req.query("patient_id", query.patient_id.trim());
        if let Some(part) = query.body_part.as_deref().filter(|s| !s.trim().is_empty()) {
            req = req.query("body_part", part.trim());
        }
        if let Some(since) = query.since.as_deref().filter(|s| !s.trim().is_empty()) {
            req = req.query("since", since.trim());
        }

        let started = Instant::now();
        let response = match req.call() {
            Ok(resp) => resp,
            Err(ureq::Error::Status(status, resp)) => {
                let body = read_body(resp);
                return Err(classify_status(status, &body));
            }
            Err(ureq::Error::Transport(transport)) => {
                return Err(classify_transport(&transport, started.elapsed(), self.timeout));
            }
        };

        let body = read_body(response);
        parse_reports(&body)
    }
}

/// 读取响应体（读不到就是「响应体读不出来」，不是「不是 JSON」—— 两者分开记）。
fn read_body(response: ureq::Response) -> String {
    match response.into_string() {
        Ok(text) => text,
        Err(err) => {
            // 读不出正文 —— 交由调用方判成 bad_response；这里保留原文供排查。
            format!("__URS_BODY_UNREADABLE__{}", err)
        }
    }
}

/// 解析响应体：先判是不是 JSON，再判字段合不合契约。
///
/// 顶层接受**数组**或**单个对象**（对象归一成单元素数组）。
fn parse_reports(body: &str) -> Result<Vec<RisReport>, RisError> {
    if let Some(rest) = body.strip_prefix("__URS_BODY_UNREADABLE__") {
        return Err(RisError::new(
            RisErrorKind::BadResponse,
            Some(DetailCode::RisBodyUnreadable),
            format!("读取 RIS 响应体失败：{}", rest),
        ));
    }
    let value: serde_json::Value = serde_json::from_str(body).map_err(|err| {
        RisError::new(
            RisErrorKind::BadResponse,
            Some(DetailCode::RisBodyNotJson),
            format!("RIS 响应体不是 JSON：{}", err),
        )
    })?;
    let items: Vec<serde_json::Value> = match value {
        serde_json::Value::Array(items) => items,
        single @ serde_json::Value::Object(_) => vec![single],
        other => {
            return Err(RisError::new(
                RisErrorKind::BadResponse,
                Some(DetailCode::RisBodySchemaInvalid),
                format!(
                    "RIS 响应体顶层不是报告对象或数组：{}",
                    truncate(&other.to_string())
                ),
            ))
        }
    };
    let mut reports = Vec::with_capacity(items.len());
    for item in items {
        let report: RisReport = serde_json::from_value(item).map_err(|err| {
            RisError::new(
                RisErrorKind::BadResponse,
                Some(DetailCode::RisBodySchemaInvalid),
                format!("RIS 响应体字段不符合接口契约：{}", err),
            )
        })?;
        reports.push(report);
    }
    Ok(reports)
}

/// 按 HTTP 状态码判定错误语义（**纯函数**，便于逐条断言）。
pub fn classify_status(status: u16, body: &str) -> RisError {
    let snippet = truncate(body);
    match status {
        401 | 403 => RisError::new(
            RisErrorKind::AuthFailed,
            Some(DetailCode::RisUnauthorized),
            format!("RIS 返回 {}（令牌无效或无权限）：{}", status, snippet),
        ),
        s if s >= 500 => RisError::new(
            RisErrorKind::UpstreamUnavailable,
            Some(DetailCode::RisServerError),
            format!("RIS 返回 {}（上游服务自身出错）：{}", s, snippet),
        ),
        s if s >= 400 => RisError::new(
            RisErrorKind::BadResponse,
            Some(DetailCode::RisClientError),
            format!("RIS 返回 {}（请求被拒）：{}", s, snippet),
        ),
        s => RisError::new(
            RisErrorKind::BadResponse,
            Some(DetailCode::RisClientError),
            format!("RIS 返回非预期的状态码 {}：{}", s, snippet),
        ),
    }
}

/// 按传输层错误判定错误语义（**纯函数**，便于逐条断言）。
///
/// **超时优先于「连不上」**：超时与连接失败在运维眼里是两回事 ——
/// 超时通常是 RIS 慢（大历史集查询），连不上通常是网络或服务没起。
pub fn classify_transport(
    transport: &ureq::Transport,
    elapsed: Duration,
    timeout: Duration,
) -> RisError {
    let kind = transport.kind();
    let message = transport.message().unwrap_or("");
    // ureq 把 socket 超时归一成 io::ErrorKind::TimedOut（见 ureq 的 stream/response 层）。
    let timed_out = transport
        .source()
        .and_then(|src| src.downcast_ref::<std::io::Error>())
        .map(|err| err.kind() == std::io::ErrorKind::TimedOut)
        .unwrap_or(false)
        // 兜底：耗时顶到超时上限（ureq 内部实现变了也判得出来）。
        || elapsed >= timeout;
    let reason = format!(
        "RIS 请求未完成（{:?}，耗时 {} 毫秒，上限 {} 毫秒）：{}",
        kind,
        elapsed.as_millis(),
        timeout.as_millis(),
        truncate(message)
    );
    match kind {
        ureq::ErrorKind::BadStatus | ureq::ErrorKind::BadHeader => RisError::new(
            RisErrorKind::BadResponse,
            Some(DetailCode::RisClientError),
            format!("RIS 响应报文不合 HTTP 规范：{}", reason),
        ),
        ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => RisError::new(
            RisErrorKind::BadResponse,
            Some(DetailCode::RisClientError),
            format!("RIS 端点不合法：{}", reason),
        ),
        _ if timed_out => RisError::new(
            RisErrorKind::UpstreamUnavailable,
            Some(DetailCode::RisTimeout),
            reason,
        ),
        _ => RisError::new(
            RisErrorKind::UpstreamUnavailable,
            Some(DetailCode::RisConnectionFailed),
            reason,
        ),
    }
}

/// 数值反序列化：JSON 数字与数字字符串都收（院内 RIS 两种都出现过）。
fn de_opt_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrText {
        Num(f64),
        Text(String),
    }
    match Option::<NumOrText>::deserialize(deserializer)? {
        None => Ok(None),
        Some(NumOrText::Num(n)) => Ok(Some(n)),
        Some(NumOrText::Text(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed
                    .parse::<f64>()
                    .map(Some)
                    .map_err(serde::de::Error::custom)
            }
        }
    }
}

/// 截断长文本（失败原因进清单，不该带着整个响应体走）。
fn truncate(text: &str) -> String {
    const LIMIT: usize = 200;
    let cleaned = text.trim();
    if cleaned.chars().count() <= LIMIT {
        return cleaned.to_string();
    }
    let head: String = cleaned.chars().take(LIMIT).collect();
    format!("{}…（已截断）", head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(base: &str, token: &str) -> RisClient {
        RisClient::new(base, token, Duration::from_secs(1)).unwrap()
    }

    #[test]
    fn 端点按基准路径拼出() {
        let c = client("https://ris.intranet", "t");
        assert_eq!(c.endpoint(), "https://ris.intranet/api/v1/ultrasound-reports");
        // 末尾斜杠不产生双斜杠。
        let c2 = client("https://ris.intranet/", "t");
        assert_eq!(c2.endpoint(), "https://ris.intranet/api/v1/ultrasound-reports");
        // 端点自带前缀路径时保留前缀。
        let c3 = client("https://ris.intranet/his", "t");
        assert_eq!(c3.endpoint(), "https://ris.intranet/his/api/v1/ultrasound-reports");
    }

    #[test]
    fn 端点或凭据缺失判未配置() {
        let err = RisClient::new("", "t", Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind, RisErrorKind::NotConfigured);
        assert_eq!(err.code(), "RIS_NOT_CONFIGURED");
        assert!(err.reason.contains("RIS_BASE_URL"));

        let err = RisClient::new("https://ris.intranet", "  ", Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind, RisErrorKind::NotConfigured);
        assert!(err.reason.contains("RIS_API_TOKEN"));

        let err = RisClient::new("ftp://ris", "t", Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind, RisErrorKind::NotConfigured);
    }

    #[test]
    fn 四类语义各自映射到共用码表里的码() {
        assert_eq!(
            RisErrorKind::UpstreamUnavailable.error_code(),
            ErrorCode::RisUpstreamUnavailable
        );
        assert_eq!(RisErrorKind::AuthFailed.error_code(), ErrorCode::RisAuthFailed);
        assert_eq!(RisErrorKind::BadResponse.error_code(), ErrorCode::RisBadResponse);
        assert_eq!(
            RisErrorKind::NotConfigured.error_code(),
            ErrorCode::RisNotConfigured
        );
        // 语义 → 码 → 码表反查，三跳一致（码确实登记在共用表里）。
        for kind in RisErrorKind::ALL {
            let code = kind.error_code();
            assert_eq!(ErrorCode::lookup(code.as_str()), Some(code));
        }
        // 只有「上游不可达」可重试。
        assert!(RisErrorKind::UpstreamUnavailable.retryable());
        assert!(!RisErrorKind::AuthFailed.retryable());
        assert!(!RisErrorKind::BadResponse.retryable());
        assert!(!RisErrorKind::NotConfigured.retryable());
    }

    #[test]
    fn 状态码分类逐条对得上() {
        // 401 / 403 → 鉴权失败（不可重试）。
        for status in [401u16, 403] {
            let err = classify_status(status, "denied");
            assert_eq!(err.kind, RisErrorKind::AuthFailed);
            assert_eq!(err.detail, Some(DetailCode::RisUnauthorized));
            assert!(!err.retryable());
        }
        // 5xx → 上游不可达（可重试）。
        for status in [500u16, 502, 503, 504] {
            let err = classify_status(status, "boom");
            assert_eq!(err.kind, RisErrorKind::UpstreamUnavailable);
            assert_eq!(err.detail, Some(DetailCode::RisServerError));
            assert!(err.retryable());
        }
        // 其他 4xx → 响应不合契约（不可重试）。
        for status in [400u16, 404, 422] {
            let err = classify_status(status, "nope");
            assert_eq!(err.kind, RisErrorKind::BadResponse);
            assert_eq!(err.detail, Some(DetailCode::RisClientError));
            assert!(!err.retryable());
        }
        // 非预期状态码（2xx 但不是 200 之类）也不静默放过。
        let err = classify_status(204, "");
        assert_eq!(err.kind, RisErrorKind::BadResponse);
    }

    #[test]
    fn 响应体按契约解析_数组与单对象都收() {
        let body = r#"[
            {"study_no":"S-1","patient_id":"PID-1","body_part":"kidney.left",
             "performed_at":"2026-03-01","measurements":[{"item_code":"kidney.ap_diameter","value":28,"unit":"mm"}]},
            {"study_no":"S-2","patient_id":"PID-1","body_part":"kidney.left",
             "performed_at":"2026-01-01","measurements":[]}
        ]"#;
        let reports = parse_reports(body).unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].study_no, "S-1");
        assert_eq!(reports[0].measurements.len(), 1);
        assert_eq!(reports[0].measurements[0].item_code, "kidney.ap_diameter");
        assert_eq!(reports[0].measurements[0].value, Some(28.0));
        assert_eq!(reports[1].measurements.len(), 0);

        // 单对象按「只有一份历史报告」归一成单元素数组。
        let single = r#"{"study_no":"S-3","measurements":[]}"#;
        let reports = parse_reports(single).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].study_no, "S-3");
    }

    #[test]
    fn 数值给数字或给字符串都收() {
        let body = r#"{"study_no":"S-1","measurements":[
            {"item_code":"a","value":28,"unit":"mm"},
            {"item_code":"b","value":"32.5","unit":"mm"},
            {"item_code":"c","value":null},
            {"item_code":"d"}
        ]}"#;
        let reports = parse_reports(body).unwrap();
        let ms = &reports[0].measurements;
        assert_eq!(ms[0].value, Some(28.0));
        assert_eq!(ms[1].value, Some(32.5));
        assert_eq!(ms[2].value, None);
        assert_eq!(ms[3].value, None);
        assert_eq!(ms[3].unit, None);
    }

    #[test]
    fn 响应体不合契约分别判两种错() {
        // 不是 JSON。
        let err = parse_reports("<html>502 Bad Gateway</html>").unwrap_err();
        assert_eq!(err.kind, RisErrorKind::BadResponse);
        assert_eq!(err.detail, Some(DetailCode::RisBodyNotJson));
        assert_eq!(err.code(), "RIS_BAD_RESPONSE");

        // 是 JSON 但字段不合契约（缺 study_no）。
        let err = parse_reports(r#"[{"patient_id":"P","measurements":[]}]"#).unwrap_err();
        assert_eq!(err.kind, RisErrorKind::BadResponse);
        assert_eq!(err.detail, Some(DetailCode::RisBodySchemaInvalid));

        // 顶层不是对象也不是数组。
        let err = parse_reports("42").unwrap_err();
        assert_eq!(err.detail, Some(DetailCode::RisBodySchemaInvalid));
    }

    #[test]
    fn 缺患者主索引直接判失败不发请求() {
        let c = client("https://ris.intranet", "t");
        let err = c.fetch_history(&RisQuery::default()).unwrap_err();
        assert_eq!(err.kind, RisErrorKind::BadResponse);
        assert!(err.reason.contains("patient_id"));
    }

    #[test]
    fn 退避重试上限截到两次() {
        let c = client("https://ris.intranet", "t").with_max_retries(99);
        assert_eq!(c.max_retries(), DEFAULT_MAX_RETRIES);
        let c = client("https://ris.intranet", "t").with_max_retries(0);
        assert_eq!(c.max_retries(), 0);
    }

    #[test]
    fn 失败条目带共用码与明细位置为零不编造() {
        let err = classify_status(503, "busy");
        let failure = err.to_failure();
        assert_eq!(failure.code, ErrorCode::RisUpstreamUnavailable);
        assert_eq!(failure.detail_code(), Some(DetailCode::RisServerError));
        // 出站失败不在报告正文里 —— 行列一律 0，不编造。
        assert_eq!(failure.line, 0);
        assert_eq!(failure.column, 0);
        assert_eq!(failure.char_start, 0);
        // 处置动作是「待上游恢复」，不是「整份退回」。
        assert_eq!(failure.disposition(), crate::parse::errors::Disposition::AwaitingUpstream);
        assert!(!failure.policy().blocks_report);
    }
}
