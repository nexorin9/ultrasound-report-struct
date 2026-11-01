//! EMR 结构化报告回写客户端 —— **出站写**（本项目三条真实出站之一）。
//!
//! **这条出站是干什么的**：超声医师逐条确认之后，本产品把**结构化结论**写进
//! 院内 EMR 的结构化报告接口 —— 这是本项目**改变真实业务状态**的那一步：
//! 台账与差值留在本地状态库里只是「算出来了」，写进 EMR 才是「病历里有这条结论」。
//!
//! **接口契约**（与 `spec.md` §2.3 一致）：
//!
//! ```text
//! POST {EMR_BASE_URL}/api/v1/structured-reports
//! Authorization: Bearer $EMR_API_TOKEN
//! Idempotency-Key: sha256(study_no‖报告内容)
//! Content-Type: application/json
//! ```
//!
//! 请求体字段：`{study_no, patient_id, items[], comparisons[], dictionary_version}`。
//!
//! ## 只发结构化结论（外发不含原始可识别文本）
//!
//! 请求体里**只有**：部位码 / 测项码 / 数值 / 单位 / 强转路径 / 位置 / 差值 / 词典版本。
//! **没有任何字段能装下原始报告段落文本** —— 这不是靠「记得别放」，是结构上放不下：
//! 报文由 [`build_payload`] 从**结构化字段**装配，调用点根本没有原文可传。
//!
//! 另有两道防线：
//!
//! 1. **词典键必须是码**：[`build_payload`] 拒绝含非 ASCII 字符的部位键 / 测项码 ——
//!    词典被误配成中文词时，叙述文本就会顺着键流进外发报文。
//! 2. **出站报文扫描**：[`scan_forbidden_text`] 把报告原文里的叙述片段
//!    拿去比对报文，命中即判失败（`assert_no_raw_text`）。
//!
//! ## 凭据只从环境变量读
//!
//! `EMR_API_TOKEN` / `EMR_BASE_URL` 都走环境变量；配置文件里出现密钥形状的键
//! **启动即拒**（见 `config.rs`）—— 配置文件常常被人打包发到科室群里。
//!
//! ## 失败四分类（与出站读同一套口径）
//!
//! | 语义 | 触发 | 可重试 | 稳定错误码 | 明细归类 |
//! |---|---|---|---|---|
//! | `upstream_unavailable` | 连不上 / 超时 / 5xx | 是 | `EMR_UPSTREAM_UNAVAILABLE` | `EMR_TIMEOUT` / `EMR_CONNECTION_FAILED` / `EMR_SERVER_ERROR` |
//! | `auth_failed` | 401 / 403 | 否 | `EMR_AUTH_FAILED` | `EMR_UNAUTHORIZED` |
//! | `bad_response` | 其他 4xx / 请求本身不合法 | 否 | `EMR_BAD_RESPONSE` | `EMR_CLIENT_ERROR` / `EMR_REQUEST_INVALID` |
//! | `not_configured` | 端点或凭据缺失（还没发请求） | 否 | `EMR_NOT_CONFIGURED` | — |
//!
//! **退避重试不在这一层**：重试与「重试耗尽转 `pending_writeback`」是回写通道的策略，
//! 由 `src/outbound/retry.rs` 承担 —— 本层只负责「发一次、把失败讲清楚」。
//!
//! **错误码不另造一套**：四类语义映射到**全项目共用的那张稳定码表**
//! （[`crate::parse::errors`]）—— 运维按错误码聚合时，
//! 「EMR 写不进去」和「测值缺单位」在同一张表里，不用记两套规则。

use std::error::Error as StdError;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::compare::states::ComparisonVerdict;
use crate::config::{
    Config, Credentials, DEFAULT_REQUEST_TIMEOUT_SECS, ENV_EMR_API_TOKEN, ENV_EMR_BASE_URL,
};
use crate::outbound::{read_body, truncate};
use crate::parse::errors::{DetailCode, ErrorCode, Failure};

/// EMR 结构化报告接口路径（相对 `EMR_BASE_URL`）。
pub const EMR_STRUCTURED_REPORT_PATH: &str = "/api/v1/structured-reports";

/// 原文片段的最小长度（去空白后按字符计）。
///
/// 短于这个长度的片段不拦 —— 「cm」「mm」这类单位词本身就在报文里
/// （单位码是契约字段），拦了就是误报。
pub const MIN_FORBIDDEN_LEN: usize = 4;

/// **一条结构化结论**（外发报文里的一个测值）。
///
/// **只有码与数** —— 没有「原始写法」这一位：原值原单位是报告里的东西，
/// 外发统一用院内口径（`value` + `unit_code`），口径由 `coercion_path` 留痕。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StructuredItem {
    /// 规范部位键（`kidney.left` 等）。
    pub part_key: String,
    /// 测项码（`kidney.ap_diameter` 等）。
    pub item_code: String,
    /// 归一值（长度 mm / 容量 mL）。
    pub value: f64,
    /// 归一单位码（UCUM）。
    pub unit_code: String,
    /// 强转路径（`UnitConvert` / `DirectScale` / `Composite`）—— 口径留痕。
    pub coercion_path: String,
    /// 1 基行号（原文位置，**只到行列，不带原文**）。
    pub line: usize,
    /// 1 基列号（原文位置）。
    pub column: usize,
}

/// **一条差值结论**（当前值 vs 同侧历史值）。
///
/// 字段与 `spec.md` §2.3 一致：`{item_code, previous_value, delta}`。
/// 测项码本身就是「部位基名 + 测项后缀」（`kidney.ap_diameter`），
/// 所以只带测项码也认得出是哪个部位的哪个测项。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StructuredComparison {
    /// 测项码。
    pub item_code: String,
    /// 历史值（归一后）。
    pub previous_value: f64,
    /// 差值 = 当前值 − 历史值。
    pub delta: f64,
}

/// **外发报文**（`spec.md` §2.3 的请求体）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StructuredReportPayload {
    /// 检查号（工作站与 RIS / EMR 同源，跨系统精确匹配键）。
    pub study_no: String,
    /// 患者主索引（跨系统匹配键；契约字段）。
    pub patient_id: Option<String>,
    /// 结构化测值列表。
    pub items: Vec<StructuredItem>,
    /// 差值列表（**只含有历史的那几条** —— 没比成的没有差值可说）。
    pub comparisons: Vec<StructuredComparison>,
    /// 词典版本（口径追溯）。
    pub dictionary_version: String,
}

/// **报文装配结果**：报文本体 + 被跳过的条目数。
///
/// **跳过数要一并带出去**：认不出的条目不会进 EMR，
/// 但它得**看得见** —— 否则「这份报告写进 EMR 的是 3 条，为什么不是 5 条」
/// 只能靠人去翻失败清单。
#[derive(Clone, Debug, PartialEq)]
pub struct PayloadBuild {
    /// 报文本体。
    pub payload: StructuredReportPayload,
    /// 因键不全 / 键含原文而未进报文的条目数。
    pub skipped_items: usize,
    /// 因状态不是「有历史」而未进报文的差值条数。
    pub skipped_comparisons: usize,
}

/// 装配外发报文。
///
/// **只收结构化字段**：本函数**不接受报告原文** —— 报文里没有放原文的地方。
///
/// 条目准入（与台账口径一致）：
///
/// | 条目状态 | 进不进报文 |
/// |---|---|
/// | 部位键 + 测项码 + 归一值 + 单位码 齐 | 进 |
/// | 部位键或测项码缺（词典未命中） | **不进**（已在失败清单里） |
/// | 无归一值（查不到换算路径） | **不进**（同上） |
/// | 部位键 / 测项码含非 ASCII 字符 | **不进**（词典被误配成中文词 —— 见下） |
///
/// **为什么拒绝含中文的键**：词典文件是运营能改的。一旦有人把部位键写成
/// 「左肾积水」这种叙述词，叙述文本就会顺着键流进外发报文 ——
/// 这是本产品**唯一**能让原文漏出去的路径，所以在这里堵死。
pub fn build_payload(
    study_no: &str,
    patient_id: Option<&str>,
    items: &[crate::parse::extract::ParsedItem],
    verdicts: &[ComparisonVerdict],
    fallback_dictionary_version: &str,
) -> PayloadBuild {
    let mut out_items: Vec<StructuredItem> = Vec::new();
    let mut skipped_items = 0usize;
    let mut version = fallback_dictionary_version.to_string();

    for item in items {
        let part_key = match item.part_key.as_deref() {
            Some(key) if !key.trim().is_empty() => key.trim().to_string(),
            _ => {
                skipped_items += 1;
                continue;
            }
        };
        let item_code = match item.item_code.as_deref() {
            Some(code) if !code.trim().is_empty() => code.trim().to_string(),
            _ => {
                skipped_items += 1;
                continue;
            }
        };
        // 词典键必须是码 —— 含非 ASCII 字符就不放行（叙述文本漏出去的唯一路径）。
        if contains_non_ascii(&part_key) || contains_non_ascii(&item_code) {
            skipped_items += 1;
            continue;
        }
        let value = match item.value_si {
            Some(value) if value.is_finite() => value,
            _ => {
                skipped_items += 1;
                continue;
            }
        };
        let unit_code = match item.unit_code.as_deref() {
            Some(code) if !code.trim().is_empty() => code.trim().to_string(),
            _ => {
                skipped_items += 1;
                continue;
            }
        };
        if version.is_empty() && !item.dictionary_version.is_empty() {
            version = item.dictionary_version.clone();
        }
        out_items.push(StructuredItem {
            part_key,
            item_code,
            value,
            unit_code,
            coercion_path: item.coercion_path.as_str().to_string(),
            line: item.line,
            column: item.column,
        });
    }

    let mut comparisons: Vec<StructuredComparison> = Vec::new();
    let mut skipped_comparisons = 0usize;
    for verdict in verdicts {
        match (&verdict.delta, verdict.state) {
            (Some(delta), crate::compare::states::ComparisonState::HasHistory) => {
                let item_code = verdict.key.item_code.trim().to_string();
                if item_code.is_empty() || contains_non_ascii(&item_code) {
                    skipped_comparisons += 1;
                    continue;
                }
                comparisons.push(StructuredComparison {
                    item_code,
                    previous_value: delta.previous_value,
                    delta: delta.delta,
                });
            }
            _ => skipped_comparisons += 1,
        }
    }

    PayloadBuild {
        payload: StructuredReportPayload {
            study_no: study_no.to_string(),
            patient_id: patient_id.map(|p| p.to_string()),
            items: out_items,
            comparisons,
            dictionary_version: version,
        },
        skipped_items,
        skipped_comparisons,
    }
}

/// 是否含非 ASCII 字符（含 CJK 汉字与全角标点）。
fn contains_non_ascii(text: &str) -> bool {
    !text.is_ascii()
}

/// **从报告原文里提取「不得出现在外发报文里」的片段**。
///
/// **判据**（两条同时满足才算叙述片段）：
///
/// 1. 去掉空白后长度 ≥ [`MIN_FORBIDDEN_LEN`]；
/// 2. 含**非 ASCII 字符**（中文叙述 / 全角标点）。
///
/// 第 2 条把「数值 / 单位 / 编码 / 标识」放行 —— 那些是结构化字段的词汇，
/// 报文里出现它们是**契约要求的**（`value` / `unit_code` / `study_no` / `patient_id`），
/// 拦了就是误报。而**中文叙述**才是真正不能外发的：它可能带患者可识别信息。
pub fn forbidden_fragments(raw_text: &str) -> Vec<String> {
    let mut fragments = Vec::new();
    for line in raw_text.lines() {
        let normalized: String = line.chars().filter(|ch| !ch.is_whitespace()).collect();
        if normalized.chars().count() < MIN_FORBIDDEN_LEN {
            continue;
        }
        if !contains_non_ascii(&normalized) {
            continue;
        }
        if fragments.iter().any(|f: &String| f == &normalized) {
            continue;
        }
        fragments.push(normalized);
    }
    fragments
}

/// **扫描外发报文里有没有混进原文叙述片段**。
///
/// 返回命中的片段（**空 = 干净**）。报文与片段都比对前先去空白 ——
/// 报文里的换行与缩进不该影响判定。
pub fn scan_forbidden_text(payload: &str, raw_text: &str) -> Vec<String> {
    let normalized_payload: String = payload.chars().filter(|ch| !ch.is_whitespace()).collect();
    forbidden_fragments(raw_text)
        .into_iter()
        .filter(|fragment| normalized_payload.contains(fragment.as_str()))
        .collect()
}

/// 断言外发报文不含原文叙述片段（命中即报出是哪几段）。
pub fn assert_no_raw_text(payload: &str, raw_text: &str) -> Result<(), String> {
    let hits = scan_forbidden_text(payload, raw_text);
    if hits.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "外发报文里出现了原始报告叙述片段：{}",
            hits.join("；")
        ))
    }
}

/// 出站写失败的四分类语义。
///
/// **只有这四类** —— 出站写的失败再多也就这几种，留自由文本分支就没法按码聚合。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmrErrorKind {
    /// 上游不可达：连接被拒 / 超时 / 5xx。**可退避重试。**
    UpstreamUnavailable,
    /// 鉴权失败：401 / 403。重试没意义，要人去换令牌。
    AuthFailed,
    /// 响应不合契约：其他 4xx / 请求本身不合法。
    BadResponse,
    /// 端点或凭据未配置：还没发请求就失败了。
    NotConfigured,
}

impl EmrErrorKind {
    /// 语义字符串（进日志与失败原因，便于按语义聚合）。
    pub fn as_str(self) -> &'static str {
        match self {
            EmrErrorKind::UpstreamUnavailable => "upstream_unavailable",
            EmrErrorKind::AuthFailed => "auth_failed",
            EmrErrorKind::BadResponse => "bad_response",
            EmrErrorKind::NotConfigured => "not_configured",
        }
    }

    /// 语义 → **全项目共用的**稳定错误码（出站写不另造一套码）。
    pub fn error_code(self) -> ErrorCode {
        match self {
            EmrErrorKind::UpstreamUnavailable => ErrorCode::EmrUpstreamUnavailable,
            EmrErrorKind::AuthFailed => ErrorCode::EmrAuthFailed,
            EmrErrorKind::BadResponse => ErrorCode::EmrBadResponse,
            EmrErrorKind::NotConfigured => ErrorCode::EmrNotConfigured,
        }
    }

    /// 是否值得退避重试。
    ///
    /// 只有「上游不可达」值得 —— 鉴权失败重试令牌也不会变好，
    /// 请求不合契约重试只会再错一次。
    pub fn retryable(self) -> bool {
        matches!(self, EmrErrorKind::UpstreamUnavailable)
    }

    /// 全部语义（穷举清单，便于测试逐条覆盖）。
    pub const ALL: [EmrErrorKind; 4] = [
        EmrErrorKind::UpstreamUnavailable,
        EmrErrorKind::AuthFailed,
        EmrErrorKind::BadResponse,
        EmrErrorKind::NotConfigured,
    ];
}

/// 出站写失败。
#[derive(Clone, Debug, PartialEq)]
pub struct EmrError {
    /// 四分类语义。
    pub kind: EmrErrorKind,
    /// 明细归类（同一语义下的具体坏法；`not_configured` 没有明细）。
    pub detail: Option<DetailCode>,
    /// 中文原因（进失败清单与运维摘要）。
    pub reason: String,
    /// 已尝试次数（1 = 只试过一次就没再试）。
    pub attempts: usize,
}

impl EmrError {
    /// 造一条出站写失败。
    pub fn new(
        kind: EmrErrorKind,
        detail: Option<DetailCode>,
        reason: impl Into<String>,
    ) -> EmrError {
        EmrError {
            kind,
            detail,
            reason: reason.into(),
            attempts: 1,
        }
    }

    /// 未配置（端点或凭据缺失）。
    pub fn not_configured(reason: impl Into<String>) -> EmrError {
        EmrError::new(EmrErrorKind::NotConfigured, None, reason)
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
    pub fn with_attempts(mut self, attempts: usize) -> EmrError {
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

impl std::fmt::Display for EmrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl StdError for EmrError {}

/// **EMR 回执**（写成功后的凭据）。
#[derive(Clone, Debug, PartialEq)]
pub struct EmrWriteReceipt {
    /// EMR 返回的状态码。
    pub status: u16,
    /// EMR 回执里的结构化报告 id（EMR 给了才有）。
    pub receipt_id: Option<String>,
    /// 响应体片段（截断，供运维排查）。
    pub body_snippet: String,
}

/// **幂等键** = `sha256(study_no ‖ 报告内容)`。
///
/// 与入库通道**同一把钥匙、同一套算法**（`crate::workflow::idempotency`）——
/// 幂等键只能有一个算法、一个拼接顺序，散开写迟早会写出两个版本。
pub fn idempotency_key(study_no: &str, raw_body: &str) -> String {
    crate::workflow::idempotency::idempotency_key_text(study_no, raw_body)
}

/// EMR 结构化报告回写客户端。
///
/// **可克隆、可跨线程** —— 守护进程里多个 worker 共用一个客户端，
/// 连接池（ureq 的 agent）由它内部管着。
#[derive(Clone, Debug)]
pub struct EmrClient {
    agent: ureq::Agent,
    base_url: String,
    token: String,
    timeout: Duration,
}

impl EmrClient {
    /// 用给定端点与凭据建客户端。
    ///
    /// **端点或凭据缺失 → 显式失败**（`EMR_NOT_CONFIGURED`），
    /// 不静默用一个默认端点糊弄过去 —— 那样运维会以为连的是真 EMR。
    pub fn new(base_url: &str, token: &str, timeout: Duration) -> Result<EmrClient, EmrError> {
        let base = base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(EmrError::not_configured(format!(
                "{} 未设置（出站写端点缺失）",
                ENV_EMR_BASE_URL
            )));
        }
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(EmrError::not_configured(format!(
                "{} 不是 http(s) 端点：{}",
                ENV_EMR_BASE_URL, base
            )));
        }
        if token.trim().is_empty() {
            return Err(EmrError::not_configured(format!(
                "{} 未设置（出站写凭据缺失）",
                ENV_EMR_API_TOKEN
            )));
        }
        let agent = ureq::AgentBuilder::new().timeout(timeout).build();
        Ok(EmrClient {
            agent,
            base_url: base.to_string(),
            token: token.trim().to_string(),
            timeout,
        })
    }

    /// 从**进程环境变量**建客户端（端点 `EMR_BASE_URL`、凭据 `EMR_API_TOKEN`）。
    ///
    /// 凭据只走环境变量 —— 配置文件里不落密钥。
    pub fn from_env() -> Result<EmrClient, EmrError> {
        let base = std::env::var(ENV_EMR_BASE_URL).unwrap_or_default();
        let token = std::env::var(ENV_EMR_API_TOKEN).unwrap_or_default();
        EmrClient::new(
            &base,
            &token,
            Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
        )
    }

    /// 从配置 + 凭据建客户端（端点取配置 / 环境变量，凭据只取环境变量）。
    pub fn from_config(config: &Config, creds: &Credentials) -> Result<EmrClient, EmrError> {
        let base = config.emr_base_url.clone().unwrap_or_default();
        let token = creds.emr_api_token.clone().unwrap_or_default();
        EmrClient::new(
            &base,
            &token,
            Duration::from_secs(config.request_timeout_secs),
        )
    }

    /// 出站写端点（全路径）。
    pub fn endpoint(&self) -> String {
        format!("{}{}", self.base_url, EMR_STRUCTURED_REPORT_PATH)
    }

    /// 请求超时上限。
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// **回写一次**（单次尝试；退避重试由回写通道承担）。
    ///
    /// 出站请求头：
    ///
    /// - `Authorization: Bearer $EMR_API_TOKEN`（凭据来自环境变量）；
    /// - `Idempotency-Key: sha256(study_no‖报告内容)`（**契约算法**）；
    /// - `Content-Type: application/json`。
    pub fn send(
        &self,
        payload: &StructuredReportPayload,
        idempotency_key: &str,
    ) -> Result<EmrWriteReceipt, EmrError> {
        // 请求本身先过关：没有幂等键就没有去重保证，没有检查号就认不出是哪份报告。
        if idempotency_key.trim().is_empty() {
            return Err(EmrError::new(
                EmrErrorKind::BadResponse,
                Some(DetailCode::EmrRequestInvalid),
                "Idempotency-Key 不能为空（回写请求缺去重保证）",
            ));
        }
        if payload.study_no.trim().is_empty() {
            return Err(EmrError::new(
                EmrErrorKind::BadResponse,
                Some(DetailCode::EmrRequestInvalid),
                "study_no 不能为空（认不出是哪份报告）",
            ));
        }
        if payload.items.is_empty() {
            // 一条结论都没有还往 EMR 里写 —— 只会多出一张空报告。
            // 认不出的条目在失败清单里，不该靠一条空回写「表示处理过了」。
            return Err(EmrError::new(
                EmrErrorKind::BadResponse,
                Some(DetailCode::EmrRequestInvalid),
                "没有可回写的结构化结论（条目数为 0），不回写空报告",
            ));
        }

        let body = serde_json::to_string(payload).map_err(|err| {
            EmrError::new(
                EmrErrorKind::BadResponse,
                Some(DetailCode::EmrRequestInvalid),
                format!("请求体序列化失败：{}", err),
            )
        })?;

        let started = Instant::now();
        let response = match self
            .agent
            .post(&self.endpoint())
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .set("Idempotency-Key", idempotency_key.trim())
            .send_string(&body)
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(status, resp)) => {
                let body = read_body(resp);
                return Err(classify_status(status, &body));
            }
            Err(ureq::Error::Transport(transport)) => {
                return Err(classify_transport(&transport, started.elapsed(), self.timeout));
            }
        };

        let status = response.status();
        let text = read_body(response);
        Ok(EmrWriteReceipt {
            status,
            receipt_id: receipt_id_of(&text),
            body_snippet: truncate(&text),
        })
    }
}

/// 从响应体里取回执 id（EMR 给了才有；没给不算失败 —— 2xx 就是写成功了）。
fn receipt_id_of(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let object = value.as_object()?;
    for key in ["id", "report_id", "structured_report_id"] {
        if let Some(id) = object.get(key) {
            match id {
                serde_json::Value::String(text) if !text.trim().is_empty() => {
                    return Some(text.trim().to_string())
                }
                serde_json::Value::Number(number) => return Some(number.to_string()),
                _ => {}
            }
        }
    }
    None
}

/// 按 HTTP 状态码判定错误语义（**纯函数**，便于逐条断言）。
pub fn classify_status(status: u16, body: &str) -> EmrError {
    let snippet = truncate(body);
    match status {
        401 | 403 => EmrError::new(
            EmrErrorKind::AuthFailed,
            Some(DetailCode::EmrUnauthorized),
            format!("EMR 返回 {}（令牌无效或无权限）：{}", status, snippet),
        ),
        s if s >= 500 => EmrError::new(
            EmrErrorKind::UpstreamUnavailable,
            Some(DetailCode::EmrServerError),
            format!("EMR 返回 {}（上游服务自身出错）：{}", s, snippet),
        ),
        s if s >= 400 => EmrError::new(
            EmrErrorKind::BadResponse,
            Some(DetailCode::EmrClientError),
            format!("EMR 返回 {}（请求被拒）：{}", s, snippet),
        ),
        s => EmrError::new(
            EmrErrorKind::BadResponse,
            Some(DetailCode::EmrClientError),
            format!("EMR 返回非预期的状态码 {}：{}", s, snippet),
        ),
    }
}

/// 按传输层错误判定错误语义（**纯函数**，便于逐条断言）。
///
/// **超时优先于「连不上」**：超时与连接失败在运维眼里是两回事 ——
/// 超时通常是 EMR 侧处理慢，连不上通常是网络或服务没起。
pub fn classify_transport(
    transport: &ureq::Transport,
    elapsed: Duration,
    timeout: Duration,
) -> EmrError {
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
        "EMR 请求未完成（{:?}，耗时 {} 毫秒，上限 {} 毫秒）：{}",
        kind,
        elapsed.as_millis(),
        timeout.as_millis(),
        truncate(message)
    );
    match kind {
        ureq::ErrorKind::BadStatus | ureq::ErrorKind::BadHeader => EmrError::new(
            EmrErrorKind::BadResponse,
            Some(DetailCode::EmrClientError),
            format!("EMR 响应报文不合 HTTP 规范：{}", reason),
        ),
        ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => EmrError::new(
            EmrErrorKind::BadResponse,
            Some(DetailCode::EmrClientError),
            format!("EMR 端点不合法：{}", reason),
        ),
        _ if timed_out => EmrError::new(
            EmrErrorKind::UpstreamUnavailable,
            Some(DetailCode::EmrTimeout),
            reason,
        ),
        _ => EmrError::new(
            EmrErrorKind::UpstreamUnavailable,
            Some(DetailCode::EmrConnectionFailed),
            reason,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_report;

    /// 一份带叙述段的脱敏报告（叙述段是**不允许**外发的东西）。
    const REPORT: &str = "超声所见：\n左肾前后径 3.2cm\n肾盂轻度扩张\n提示：左肾积水，请结合临床。";

    #[test]
    fn 报文字段与契约一致() {
        let outcome = parse_report("左肾前后径 3.2cm");
        let build = build_payload(
            "SAMPLE-URO-001",
            Some("PID-EXAMPLE"),
            &outcome.items,
            &[],
            &outcome.dictionary_version,
        );
        assert_eq!(build.payload.study_no, "SAMPLE-URO-001");
        assert_eq!(build.payload.patient_id.as_deref(), Some("PID-EXAMPLE"));
        assert_eq!(build.payload.items.len(), 1);
        let item = &build.payload.items[0];
        assert_eq!(item.part_key, "kidney.left");
        assert_eq!(item.item_code, "kidney.ap_diameter");
        assert_eq!(item.unit_code, "mm");
        assert_eq!(item.value, 32.0);
        assert_eq!(item.coercion_path, "UnitConvert");
        // 词典版本随条目透传。
        assert!(!build.payload.dictionary_version.is_empty());
    }

    #[test]
    fn 报文只含契约字段() {
        let outcome = parse_report("左肾积水 3.2cm");
        let build = build_payload(
            "SAMPLE-URO-001",
            Some("PID-EXAMPLE"),
            &outcome.items,
            &[],
            &outcome.dictionary_version,
        );
        let json = serde_json::to_string(&build.payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort();
        // 顶层字段严格按 spec §2.3 —— 多一个少一个都不行。
        assert_eq!(
            keys,
            vec![
                "comparisons",
                "dictionary_version",
                "items",
                "patient_id",
                "study_no"
            ]
        );
    }

    #[test]
    fn 键不全或含中文的条目不进报文且被计数() {
        let outcome = parse_report("胆囊 3.2cm");
        let build = build_payload(
            "SAMPLE-URO-002",
            None,
            &outcome.items,
            &[],
            &outcome.dictionary_version,
        );
        // 「胆囊」不在部位词典里 —— 没有部位键，进不了报文。
        assert_eq!(build.payload.items.len(), 0);
        assert_eq!(build.skipped_items, 1);

        // 词典键被误配成中文词 → 同样拦住（叙述文本不许顺着键漏出去）。
        let mut item = outcome.items[0].clone();
        item.part_key = Some("左肾积水".to_string());
        item.item_code = Some("积水深度".to_string());
        item.value_si = Some(3.2);
        item.unit_code = Some("cm".to_string());
        let build = build_payload("SAMPLE-URO-003", None, &[item], &[], "parts-0.1.0");
        assert_eq!(build.payload.items.len(), 0);
        assert_eq!(build.skipped_items, 1);
    }

    #[test]
    fn 出站报文不含原始报告叙述文本() {
        let outcome = parse_report(REPORT);
        let build = build_payload(
            "SAMPLE-URO-004",
            Some("PID-EXAMPLE"),
            &outcome.items,
            &[],
            &outcome.dictionary_version,
        );
        let json = serde_json::to_string(&build.payload).unwrap();
        let hits = scan_forbidden_text(&json, REPORT);
        assert!(hits.is_empty(), "报文里混进了原文叙述：{:?}", hits);
        assert!(assert_no_raw_text(&json, REPORT).is_ok());
    }

    #[test]
    fn 叙述片段判据拦中文放行数值单位() {
        let fragments = forbidden_fragments(REPORT);
        assert!(
            fragments.iter().any(|f| f.contains("左肾积水")),
            "叙述片段应被拦下：{:?}",
            fragments
        );
        // 纯 ASCII 的数值 / 单位 / 标识不是叙述 —— 报文里出现它们是契约要求的。
        assert!(forbidden_fragments("3.2cm\nPID-000123\nmm").is_empty());
    }

    #[test]
    fn 端点或凭据缺失判未配置() {
        let err = EmrClient::new("", "t", Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind, EmrErrorKind::NotConfigured);
        assert_eq!(err.code(), "EMR_NOT_CONFIGURED");
        assert!(err.reason.contains("EMR_BASE_URL"));

        let err = EmrClient::new("https://emr.intranet", "  ", Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind, EmrErrorKind::NotConfigured);
        assert!(err.reason.contains("EMR_API_TOKEN"));

        let err = EmrClient::new("ftp://emr", "t", Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind, EmrErrorKind::NotConfigured);
    }

    #[test]
    fn 四类语义各自映射到共用码表里的码() {
        for kind in EmrErrorKind::ALL {
            let code = kind.error_code();
            assert_eq!(ErrorCode::lookup(code.as_str()), Some(code));
        }
        assert_eq!(
            EmrErrorKind::UpstreamUnavailable.error_code(),
            ErrorCode::EmrUpstreamUnavailable
        );
        assert_eq!(EmrErrorKind::AuthFailed.error_code(), ErrorCode::EmrAuthFailed);
        assert_eq!(EmrErrorKind::BadResponse.error_code(), ErrorCode::EmrBadResponse);
        assert_eq!(EmrErrorKind::NotConfigured.error_code(), ErrorCode::EmrNotConfigured);
        // 只有「上游不可达」可重试。
        assert!(EmrErrorKind::UpstreamUnavailable.retryable());
        assert!(!EmrErrorKind::AuthFailed.retryable());
        assert!(!EmrErrorKind::BadResponse.retryable());
        assert!(!EmrErrorKind::NotConfigured.retryable());
    }

    #[test]
    fn 幂等键与入库通道同一把钥匙() {
        let key = idempotency_key("SAMPLE-URO-001", REPORT);
        assert_eq!(
            key,
            crate::workflow::idempotency::idempotency_key_text("SAMPLE-URO-001", REPORT)
        );
        assert_eq!(key.len(), 64);
    }

    #[test]
    fn 状态码分类逐条对得上() {
        for status in [401u16, 403] {
            let err = classify_status(status, "denied");
            assert_eq!(err.kind, EmrErrorKind::AuthFailed);
            assert_eq!(err.detail, Some(DetailCode::EmrUnauthorized));
            assert!(!err.retryable());
        }
        for status in [500u16, 502, 503, 504] {
            let err = classify_status(status, "boom");
            assert_eq!(err.kind, EmrErrorKind::UpstreamUnavailable);
            assert_eq!(err.detail, Some(DetailCode::EmrServerError));
            assert!(err.retryable());
        }
        for status in [400u16, 404, 409, 422] {
            let err = classify_status(status, "nope");
            assert_eq!(err.kind, EmrErrorKind::BadResponse);
            assert_eq!(err.detail, Some(DetailCode::EmrClientError));
            assert!(!err.retryable());
        }
    }

    #[test]
    fn 请求本身不合法时不发请求() {
        let client = EmrClient::new("https://emr.intranet", "t", Duration::from_secs(1)).unwrap();
        let payload = StructuredReportPayload {
            study_no: "S-1".to_string(),
            patient_id: Some("PID-EXAMPLE".to_string()),
            items: vec![StructuredItem {
                part_key: "kidney.left".to_string(),
                item_code: "kidney.ap_diameter".to_string(),
                value: 32.0,
                unit_code: "mm".to_string(),
                coercion_path: "UnitConvert".to_string(),
                line: 2,
                column: 6,
            }],
            comparisons: Vec::new(),
            dictionary_version: "parts-0.1.0+items-0.1.0".to_string(),
        };
        // 幂等键缺。
        let err = client.send(&payload, "  ").unwrap_err();
        assert_eq!(err.kind, EmrErrorKind::BadResponse);
        assert_eq!(err.detail, Some(DetailCode::EmrRequestInvalid));
        // 条目为空 → 不回写空报告。
        let empty = StructuredReportPayload {
            items: Vec::new(),
            ..payload.clone()
        };
        let err = client.send(&empty, &idempotency_key("S-1", "左肾 3.2cm")).unwrap_err();
        assert_eq!(err.detail, Some(DetailCode::EmrRequestInvalid));
    }
}
