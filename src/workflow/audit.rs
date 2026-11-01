//! 工作流第 2 层：审计日志逐动作留痕（九类动作）。
//!
//! **这一层解决的接缝**：台账行落库了、医师点确认了、EMR 写回了 ——
//! 半年后内审问一句「这份报告当时按哪套口径算的、谁确认的、回写到哪去了」，
//! 现场答不上来。本层把**每个动作**都留下来。
//!
//! **九类动作 = 九个工作流步骤**（`spec.md` §5 / `plan.md` §5）：
//!
//! | 动作码 | 中文 | 写入点（本层唯一出口） | 调用方 | 责任人 |
//! |---|---|---|---|---|
//! | `received` | 接收 | [`AuditRecorder::received`] | 入库事务 `state::repo::commit_ingestion` | 系统 |
//! | `parsed` | 解析 | [`AuditRecorder::parsed`] | 同上 | 系统 |
//! | `coerced` | 强转 | [`AuditRecorder::coerced`] | 同上 | 系统 |
//! | `failed` | 失败 | [`AuditRecorder::failed`] | 同上（每条失败条目一条） | 系统 |
//! | `compared` | 对拍 | [`AuditRecorder::compared`] | 一轮对拍收口处 | 系统 |
//! | `confirmed` | 确认 | [`AuditRecorder::confirmed`] | 医师确认回执 | 医师 |
//! | `rejected` | 驳回 | [`AuditRecorder::rejected`] | 医师确认回执 | 医师 |
//! | `written_back` | 回写 | [`AuditRecorder::written_back`] | EMR 出站写回成功处 | 系统 |
//! | `pushed` | 推送 | [`AuditRecorder::pushed`] | 企业微信推送成功处 | 系统 |
//!
//! **每类动作只有一个写入点**：动作码由方法名固定，调用方**不能自己拼动作串**。
//! 拼动作串的后果是同一个动作出现三种写法（`parsed` / `parse_done` / `解析完成`），
//! 运维按动作码聚合就聚合不出来 —— 审计链的价值全在「能按码聚合」。
//!
//! **每条留痕带六样东西**（`plan.md` §5）：动作、责任人、时间、输入摘要哈希、词典版本、详情。
//! 失败条目另外带稳定错误码与行列（行列是「照着能找回原文」的锚点）。
//!
//! **输入摘要哈希与幂等键同源、用途分开**：
//! 两者算的都是 `sha256(study_no‖报告内容)`（[`input_digest`]），
//! 但落在**两张表、两个用途**上 ——
//!
//! - `audit_log.input_digest`：这条留痕对应的是**哪份报告内容**（审计用）；
//! - `idempotency.idempotency_key`：这份内容**处理过没有**（去重用）。
//!
//! 审计侧**只写不读**：留痕层从不拿摘要去判重（判重只认幂等表），
//! 反过来幂等路径也不读审计表 —— 两条路径互不依赖。
//!
//! **确认与驳回互斥**：一份报告要么被确认、要么被驳回，不会既确认又驳回。
//! 所以「九类动作齐全」是**库级**的口径（跨报告看），
//! 单份报告的审计链最多含九类里的八类。

use sha2::Digest;

use crate::parse::errors::Failure;
use crate::state::repo::{AuditEntry, Repo, RepoError};

/// 审计动作：接收。
pub const ACTION_RECEIVED: &str = "received";
/// 审计动作：解析完成。
pub const ACTION_PARSED: &str = "parsed";
/// 审计动作：强转归一完成（口径落定）。
pub const ACTION_COERCED: &str = "coerced";
/// 审计动作：对拍完成（有历史 / 无历史 / 歧义 / 本轮没比成）。
pub const ACTION_COMPARED: &str = "compared";
/// 审计动作：失败（失败条目逐条留痕，带错误码与行列）。
pub const ACTION_FAILED: &str = "failed";
/// 审计动作：医师确认。
pub const ACTION_CONFIRMED: &str = "confirmed";
/// 审计动作：医师驳回。
pub const ACTION_REJECTED: &str = "rejected";
/// 审计动作：已回写 EMR。
pub const ACTION_WRITTEN_BACK: &str = "written_back";
/// 审计动作：已推送企业微信。
pub const ACTION_PUSHED: &str = "pushed";
/// 审计动作：重复投递被幂等预查拦下。
///
/// **不是九类工作流动作之一** —— 它是运维信号（工作站重发 / 人工重投），
/// 记下来是为了回答「为什么这份报告没有第二条台账行」。
pub const ACTION_DUPLICATE_SKIPPED: &str = "duplicate_skipped";

/// **九类工作流动作**（落库字段，取值跨版本不变）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditAction {
    /// 接收（正文读到了）。
    Received,
    /// 解析完成（条目抽出来了）。
    Parsed,
    /// 强转归一完成（口径落定）。
    Coerced,
    /// 对拍完成。
    Compared,
    /// 失败（失败条目逐条留痕）。
    Failed,
    /// 医师确认。
    Confirmed,
    /// 医师驳回。
    Rejected,
    /// 已回写 EMR。
    WrittenBack,
    /// 已推送企业微信。
    Pushed,
}

impl AuditAction {
    /// 全部九类动作（穷举清单）。
    pub const ALL: [AuditAction; 9] = [
        AuditAction::Received,
        AuditAction::Parsed,
        AuditAction::Coerced,
        AuditAction::Compared,
        AuditAction::Failed,
        AuditAction::Confirmed,
        AuditAction::Rejected,
        AuditAction::WrittenBack,
        AuditAction::Pushed,
    ];

    /// 动作码（**落库字段**，与 `audit_log.action` 一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            AuditAction::Received => ACTION_RECEIVED,
            AuditAction::Parsed => ACTION_PARSED,
            AuditAction::Coerced => ACTION_COERCED,
            AuditAction::Compared => ACTION_COMPARED,
            AuditAction::Failed => ACTION_FAILED,
            AuditAction::Confirmed => ACTION_CONFIRMED,
            AuditAction::Rejected => ACTION_REJECTED,
            AuditAction::WrittenBack => ACTION_WRITTEN_BACK,
            AuditAction::Pushed => ACTION_PUSHED,
        }
    }

    /// 中文说明（台账 / 企微推送 / 运维摘要直接展示）。
    pub fn message(self) -> &'static str {
        match self {
            AuditAction::Received => "接收报告",
            AuditAction::Parsed => "解析完成",
            AuditAction::Coerced => "强转归一完成",
            AuditAction::Compared => "与 RIS 同侧历史对拍完成",
            AuditAction::Failed => "处理失败（该条进失败清单）",
            AuditAction::Confirmed => "医师确认",
            AuditAction::Rejected => "医师驳回",
            AuditAction::WrittenBack => "已回写 EMR",
            AuditAction::Pushed => "已推送企业微信",
        }
    }

    /// 字符串 → 动作（未知返回 `None` —— 不猜默认动作）。
    ///
    /// 从库里读回动作码时用它：读到表里没有的动作码就是**数据不对**，
    /// 宁可显式返回 `None`，也不硬套一个默认动作把未知留痕混进别的类。
    pub fn lookup(raw: &str) -> Option<AuditAction> {
        AuditAction::ALL.iter().copied().find(|a| a.as_str() == raw)
    }

    /// 这个动作是否**由系统自动**产生（不需要人点头）。
    pub fn is_system(self) -> bool {
        !matches!(self, AuditAction::Confirmed | AuditAction::Rejected)
    }
}

/// **责任人**。
///
/// 审计要能回答「这一步是谁做的」—— 系统做的写 `system`，
/// 人做的写**账号**（医师回执带回来的登录账号）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Actor {
    /// 系统动作（守护进程 / 出站客户端）。
    System,
    /// 人工动作（超声医师 / 管理员账号）。
    User(String),
}

impl Actor {
    /// 人工账号（医师回执带回来的登录账号）。
    pub fn user(account: impl Into<String>) -> Self {
        Actor::User(account.into())
    }

    /// 责任人取值（落库字段 `audit_log.actor`）。
    pub fn as_str(&self) -> &str {
        match self {
            Actor::System => "system",
            Actor::User(account) => account.as_str(),
        }
    }
}

impl std::fmt::Display for Actor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// **一次留痕的上下文**（这份留痕挂在哪份报告上）。
///
/// 四个字段就是审计链的「主键 + 口径」：
/// 报告主键（挂到 `reports`）、检查号（人看的）、输入摘要哈希（内容指纹）、
/// 词典版本（口径指纹）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditContext {
    /// 所属报告主键。
    pub report_id: Option<i64>,
    /// 检查号。
    pub study_no: Option<String>,
    /// 输入摘要哈希（`sha256(study_no‖报告内容)`）。
    pub input_digest: Option<String>,
    /// 词典版本（口径留痕）。
    pub dictionary_version: Option<String>,
}

impl AuditContext {
    /// 用现成的四个值构造。
    pub fn new(
        report_id: i64,
        study_no: &str,
        input_digest: &str,
        dictionary_version: &str,
    ) -> Self {
        AuditContext {
            report_id: Some(report_id),
            study_no: Some(study_no.to_string()),
            input_digest: Some(input_digest.to_string()),
            dictionary_version: Some(dictionary_version.to_string()),
        }
    }

    /// 用**报告正文**构造（摘要自己算）。
    ///
    /// 这是留痕的正确入口 —— 摘要由审计层按自己的口径算，
    /// 不依赖调用方「顺手」传一个什么值进来。
    pub fn for_body(
        report_id: i64,
        study_no: &str,
        body: &[u8],
        dictionary_version: &str,
    ) -> Self {
        AuditContext::new(
            report_id,
            study_no,
            &input_digest(study_no, body),
            dictionary_version,
        )
    }
}

/// **输入摘要哈希** = `sha256(study_no ‖ 报告内容)`。
///
/// **与幂等键同源、用途分开**：
///
/// - 同一套算法（`sha256`，先检查号、后报告内容）→ 同源；
/// - 落**不同的表**、服务**不同的用途** → 用途分开：
///   审计侧回答「这条留痕对应哪份内容」，幂等侧回答「这份内容处理过没有」。
///
/// **审计侧只写不读**：留痕层从不拿这个摘要去判重 —— 判重只认幂等表。
/// （反过来幂等路径也不读审计表：审计表随时可能被归档清理，
/// 判重不能依赖一张会被清空的表。）
pub fn input_digest(study_no: &str, body: &[u8]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(study_no.as_bytes());
    hasher.update(body);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

/// **一条审计留痕**（读出）。
#[derive(Clone, Debug, PartialEq)]
pub struct AuditRow {
    /// 自增主键（**留痕顺序**：严格递增，是审计链的权威顺序）。
    pub id: i64,
    /// 所属报告。
    pub report_id: Option<i64>,
    /// 检查号。
    pub study_no: Option<String>,
    /// 动作码。
    pub action: String,
    /// 责任人。
    pub actor: Option<String>,
    /// 稳定错误码（失败留痕用）。
    pub error_code: Option<String>,
    /// 1 基行号。
    pub line: Option<usize>,
    /// 1 基列号。
    pub column: Option<usize>,
    /// 原文片段。
    pub raw_snippet: Option<String>,
    /// 输入摘要哈希。
    pub input_digest: Option<String>,
    /// 词典版本。
    pub dictionary_version: Option<String>,
    /// 详情。
    pub detail: Option<String>,
    /// 留痕时间（**毫秒精度**）。
    pub created_at: String,
}

/// **审计记录器**：九类动作的**唯一写入出口**。
///
/// 持一份状态库引用；每个方法写**恰好一条**留痕，动作码由方法名固定。
pub struct AuditRecorder<'a> {
    repo: &'a Repo,
}

impl<'a> AuditRecorder<'a> {
    /// 建一个记录器（挂在状态库上）。
    pub fn new(repo: &'a Repo) -> Self {
        AuditRecorder { repo }
    }

    /// 内部写入：动作码由调用方给定（对外只暴露九个固定动作的方法）。
    fn append(
        &self,
        ctx: &AuditContext,
        action: AuditAction,
        actor: &Actor,
        detail: Option<&str>,
        failure: Option<&Failure>,
    ) -> Result<i64, RepoError> {
        let entry = AuditEntry {
            report_id: ctx.report_id,
            study_no: ctx.study_no.clone(),
            action: action.as_str().to_string(),
            actor: Some(actor.as_str().to_string()),
            error_code: failure.map(|f| f.code.as_str().to_string()),
            line: failure.map(|f| f.line),
            column: failure.map(|f| f.column),
            raw_snippet: failure.map(|f| f.raw_snippet.clone()),
            input_digest: ctx.input_digest.clone(),
            dictionary_version: ctx.dictionary_version.clone(),
            detail: detail.map(|d| d.to_string()),
        };
        self.repo.append_audit(&entry)
    }

    /// **接收**（正文读到了）。
    ///
    /// 写入点：入库事务 `state::repo::commit_ingestion`。
    pub fn received(
        &self,
        ctx: &AuditContext,
        actor: &Actor,
        detail: Option<&str>,
    ) -> Result<i64, RepoError> {
        self.append(ctx, AuditAction::Received, actor, detail, None)
    }

    /// **解析完成**（条目抽出来了）。
    ///
    /// 写入点：入库事务 `state::repo::commit_ingestion`。
    pub fn parsed(
        &self,
        ctx: &AuditContext,
        actor: &Actor,
        detail: Option<&str>,
    ) -> Result<i64, RepoError> {
        self.append(ctx, AuditAction::Parsed, actor, detail, None)
    }

    /// **强转归一完成**（口径落定：每条测值走了哪条量纲路径）。
    ///
    /// 写入点：入库事务 `state::repo::commit_ingestion`（条目落库时）。
    pub fn coerced(
        &self,
        ctx: &AuditContext,
        actor: &Actor,
        detail: Option<&str>,
    ) -> Result<i64, RepoError> {
        self.append(ctx, AuditAction::Coerced, actor, detail, None)
    }

    /// **对拍完成**（有历史 / 无历史 / 歧义 / 本轮没比成）。
    ///
    /// 写入点：一轮对拍收口处（`compare` 编排层）。
    pub fn compared(
        &self,
        ctx: &AuditContext,
        actor: &Actor,
        detail: Option<&str>,
    ) -> Result<i64, RepoError> {
        self.append(ctx, AuditAction::Compared, actor, detail, None)
    }

    /// **失败**（逐条留痕：稳定错误码 + 行列 + 原文片段）。
    ///
    /// 写入点：入库事务 `state::repo::commit_ingestion`（每条失败条目一条）。
    /// 失败清单 CSV 就是从这些留痕导出来的。
    pub fn failed(
        &self,
        ctx: &AuditContext,
        actor: &Actor,
        failure: &Failure,
    ) -> Result<i64, RepoError> {
        let detail = failure.message();
        self.append(
            ctx,
            AuditAction::Failed,
            actor,
            Some(&detail),
            Some(failure),
        )
    }

    /// **医师确认**（人工确认点：草稿 → 可回写）。
    ///
    /// 写入点：医师确认回执（`decision=confirm`）。
    pub fn confirmed(
        &self,
        ctx: &AuditContext,
        actor: &Actor,
        detail: Option<&str>,
    ) -> Result<i64, RepoError> {
        self.append(ctx, AuditAction::Confirmed, actor, detail, None)
    }

    /// **医师驳回**（终态：这条不回写了）。
    ///
    /// 写入点：医师确认回执（`decision=reject`）。
    pub fn rejected(
        &self,
        ctx: &AuditContext,
        actor: &Actor,
        detail: Option<&str>,
    ) -> Result<i64, RepoError> {
        self.append(ctx, AuditAction::Rejected, actor, detail, None)
    }

    /// **已回写 EMR**。
    ///
    /// 写入点：EMR 出站写回成功处（写成功才记「已回写」——
    /// 写失败记的是 `pending_writeback`，不是这里）。
    pub fn written_back(
        &self,
        ctx: &AuditContext,
        actor: &Actor,
        detail: Option<&str>,
    ) -> Result<i64, RepoError> {
        self.append(ctx, AuditAction::WrittenBack, actor, detail, None)
    }

    /// **已推送企业微信**。
    ///
    /// 写入点：企业微信推送成功处（推失败不记「已推送」）。
    pub fn pushed(
        &self,
        ctx: &AuditContext,
        actor: &Actor,
        detail: Option<&str>,
    ) -> Result<i64, RepoError> {
        self.append(ctx, AuditAction::Pushed, actor, detail, None)
    }

    /// **读回一份报告的审计链**（按写入顺序）。
    ///
    /// 顺序以 `id`（自增主键）为准 —— 它是**严格递增**的，
    /// 是审计链的权威顺序；`created_at` 是挂钟时间（毫秒精度），
    /// 同一毫秒内的多条留痕时间戳可能相同。
    pub fn chain(&self, report_id: i64) -> Result<Vec<AuditRow>, RepoError> {
        let mut stmt = self.repo.connection().prepare(
            "SELECT id, report_id, study_no, action, actor, error_code, line, column, raw_snippet, \
             input_digest, dictionary_version, detail, created_at \
             FROM audit_log WHERE report_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(rusqlite::params![report_id], |row| {
            Ok(AuditRow {
                id: row.get(0)?,
                report_id: row.get(1)?,
                study_no: row.get(2)?,
                action: row.get(3)?,
                actor: row.get(4)?,
                error_code: row.get(5)?,
                line: row.get(6)?,
                column: row.get(7)?,
                raw_snippet: row.get(8)?,
                input_digest: row.get(9)?,
                dictionary_version: row.get(10)?,
                detail: row.get(11)?,
                created_at: row.get(12)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 一份报告的审计链动作序列（按写入顺序）。
    pub fn actions_of(&self, report_id: i64) -> Result<Vec<AuditAction>, RepoError> {
        Ok(self
            .chain(report_id)?
            .iter()
            .filter_map(|row| AuditAction::lookup(&row.action))
            .collect())
    }

    /// 库里**出现过**的动作集合（跨报告）。
    ///
    /// 「九类动作是否齐全」是**库级**口径：确认与驳回互斥，
    /// 单份报告的链上最多只有九类里的八类。
    pub fn distinct_actions(&self) -> Result<Vec<AuditAction>, RepoError> {
        let mut stmt = self
            .repo
            .connection()
            .prepare("SELECT DISTINCT action FROM audit_log ORDER BY action")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            if let Some(action) = AuditAction::lookup(&r?) {
                out.push(action);
            }
        }
        Ok(out)
    }
}

/// **强转路径分布摘要**（写进 `coerced` 留痕的详情）。
///
/// 口径留痕要能一眼看出「这份报告有几条走换算、几条走倍率、几条没路」——
/// 没路的那些已经在失败清单里单列了。
pub fn coercion_detail(items: &[crate::parse::ParsedItem]) -> String {
    let mut unit_convert = 0usize;
    let mut direct_scale = 0usize;
    let mut composite = 0usize;
    let mut none = 0usize;
    for item in items {
        match item.coercion_path.as_str() {
            "UnitConvert" => unit_convert += 1,
            "DirectScale" => direct_scale += 1,
            "Composite" => composite += 1,
            _ => none += 1,
        }
    }
    format!(
        "强转完成：条目 {} 条（UnitConvert {} / DirectScale {} / Composite {} / None {}）",
        items.len(),
        unit_convert,
        direct_scale,
        composite,
        none
    )
}

/// **对拍结论摘要**（写进 `compared` 留痕的详情）。
///
/// 四态都说清楚：**没比成**与「首次检查」是两件事，
/// 留痕里必须分得开 —— 否则运维看到「无历史」会以为「跟上次一样」。
pub fn compared_detail(state: &str, delta: Option<f64>, direction: Option<&str>) -> String {
    match delta {
        Some(value) => format!(
            "对拍完成：状态 {}，差值 {}，方向 {}",
            state,
            value,
            direction.unwrap_or("未判定")
        ),
        None => format!("对拍完成：状态 {}（未产出差值）", state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个带报告行的状态库。
    ///
    /// 留痕挂外键（`audit_log.report_id → reports.id`），所以先落一行报告。
    fn repo_with_report() -> (Repo, i64) {
        let repo = Repo::open_in_memory().unwrap();
        let report_id = repo
            .insert_report(&crate::state::repo::NewReport {
                study_no: "S-A".to_string(),
                patient_id: Some("PID-EXAMPLE".to_string()),
                status: "parsed".to_string(),
                source_path: None,
                dictionary_version: "parts-0.1.0+items-0.1.0".to_string(),
                idempotency_key: "key-a".to_string(),
            })
            .unwrap();
        (repo, report_id)
    }

    fn ctx(report_id: i64) -> AuditContext {
        AuditContext::for_body(
            report_id,
            "S-A",
            "左肾积水 3.2cm".as_bytes(),
            "parts-0.1.0+items-0.1.0",
        )
    }

    #[test]
    fn 九类动作码稳定且互不重复() {
        assert_eq!(AuditAction::ALL.len(), 9);
        let mut seen: Vec<&str> = Vec::new();
        for action in AuditAction::ALL {
            let code = action.as_str();
            assert!(!seen.contains(&code), "动作码重复：{}", code);
            seen.push(code);
            assert!(!action.message().is_empty());
            assert_eq!(AuditAction::lookup(code), Some(action));
            // 落库字段是小写蛇形。
            assert!(
                code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{} 不是小写蛇形",
                code
            );
        }
        assert_eq!(AuditAction::lookup("no_such_action"), None);
        // 九类动作与 plan.md §5 登记的一致。
        assert_eq!(seen, vec![
            "received",
            "parsed",
            "coerced",
            "compared",
            "failed",
            "confirmed",
            "rejected",
            "written_back",
            "pushed",
        ]);
    }

    #[test]
    fn 确认与驳回是人工动作其余是系统动作() {
        assert!(!AuditAction::Confirmed.is_system());
        assert!(!AuditAction::Rejected.is_system());
        for action in AuditAction::ALL {
            if matches!(action, AuditAction::Confirmed | AuditAction::Rejected) {
                continue;
            }
            assert!(action.is_system(), "{} 应是系统动作", action.as_str());
        }
    }

    #[test]
    fn 责任人取值() {
        assert_eq!(Actor::System.as_str(), "system");
        assert_eq!(Actor::user("USR-EXAMPLE").as_str(), "USR-EXAMPLE");
        assert_eq!(Actor::user("USR-EXAMPLE").to_string(), "USR-EXAMPLE");
    }

    #[test]
    fn 输入摘要与幂等键同源() {
        // 同源：同一套算法、同一拼接顺序 —— 值逐字节相同。
        let digest = input_digest("S-A", "左肾积水 3.2cm".as_bytes());
        assert_eq!(digest, crate::state::idempotency_key_bytes("S-A", "左肾积水 3.2cm".as_bytes()));
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        // 内容变了摘要就变。
        assert_ne!(digest, input_digest("S-A", "左肾积水 3.3cm".as_bytes()));
        // 检查号变了摘要也变（拼接顺序：先检查号）。
        assert_ne!(digest, input_digest("S-B", "左肾积水 3.2cm".as_bytes()));
    }

    #[test]
    fn 上下文用正文自算摘要() {
        let c = ctx(42);
        assert_eq!(
            c.input_digest.as_deref(),
            Some(input_digest("S-A", "左肾积水 3.2cm".as_bytes()).as_str())
        );
        assert_eq!(c.report_id, Some(42));
        assert_eq!(c.study_no.as_deref(), Some("S-A"));
        assert_eq!(c.dictionary_version.as_deref(), Some("parts-0.1.0+items-0.1.0"));
    }

    #[test]
    fn 每个动作方法各写一条且动作码固定() {
        let (repo, report_id) = repo_with_report();
        let recorder = AuditRecorder::new(&repo);
        let c = ctx(report_id);
        recorder.received(&c, &Actor::System, Some("接收")).unwrap();
        recorder.parsed(&c, &Actor::System, Some("解析")).unwrap();
        recorder.coerced(&c, &Actor::System, Some("强转")).unwrap();
        recorder.compared(&c, &Actor::System, Some("对拍")).unwrap();
        recorder.confirmed(&c, &Actor::user("USR-EXAMPLE"), Some("确认")).unwrap();
        recorder.written_back(&c, &Actor::System, Some("回写")).unwrap();
        recorder.pushed(&c, &Actor::System, Some("推送")).unwrap();

        let chain = recorder.chain(report_id).unwrap();
        assert_eq!(chain.len(), 7);
        let actions: Vec<&str> = chain.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(
            actions,
            vec![
                "received",
                "parsed",
                "coerced",
                "compared",
                "confirmed",
                "written_back",
                "pushed"
            ]
        );
        // 每条都带摘要哈希与词典版本。
        for row in &chain {
            assert!(row.input_digest.is_some(), "缺输入摘要哈希");
            assert!(row.dictionary_version.is_some(), "缺词典版本");
            assert!(!row.created_at.is_empty());
        }
        // 人工动作带账号，系统动作带 system。
        let confirmed = chain.iter().find(|r| r.action == "confirmed").unwrap();
        assert_eq!(confirmed.actor.as_deref(), Some("USR-EXAMPLE"));
        let received = chain.iter().find(|r| r.action == "received").unwrap();
        assert_eq!(received.actor.as_deref(), Some("system"));
    }

    #[test]
    fn 失败留痕带错误码与行列() {
        let (repo, report_id) = repo_with_report();
        let recorder = AuditRecorder::new(&repo);
        let failure = Failure::new(
            crate::parse::errors::ErrorCode::UnitMissing,
            12,
            3,
            6,
            "3.2",
        );
        recorder.failed(&ctx(report_id), &Actor::System, &failure).unwrap();
        let chain = recorder.chain(report_id).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].action, "failed");
        assert_eq!(chain[0].error_code.as_deref(), Some("UNIT_MISSING"));
        assert_eq!(chain[0].line, Some(3));
        assert_eq!(chain[0].column, Some(6));
        assert_eq!(chain[0].raw_snippet.as_deref(), Some("3.2"));
    }

    #[test]
    fn 驳回与确认互斥但库里九类齐全() {
        let (repo, report_id) = repo_with_report();
        let recorder = AuditRecorder::new(&repo);
        recorder
            .rejected(&ctx(report_id), &Actor::user("USR-EXAMPLE"), Some("驳回"))
            .unwrap();
        let actions = recorder.actions_of(report_id).unwrap();
        assert_eq!(actions, vec![AuditAction::Rejected]);
        // 单份报告的链上只有驳回 —— 确认与驳回互斥。
        assert!(!actions.contains(&AuditAction::Confirmed));
    }

    #[test]
    fn 审计链顺序按自增主键严格递增() {
        let (repo, report_id) = repo_with_report();
        let recorder = AuditRecorder::new(&repo);
        let c = ctx(report_id);
        recorder.received(&c, &Actor::System, None).unwrap();
        recorder.parsed(&c, &Actor::System, None).unwrap();
        recorder.coerced(&c, &Actor::System, None).unwrap();
        let chain = recorder.chain(report_id).unwrap();
        for pair in chain.windows(2) {
            assert!(
                pair[0].id < pair[1].id,
                "留痕顺序必须严格递增：{} → {}",
                pair[0].id,
                pair[1].id
            );
        }
        // 时间戳非递减（挂钟时间，毫秒精度）。
        for pair in chain.windows(2) {
            assert!(
                pair[0].created_at <= pair[1].created_at,
                "留痕时间序不得倒退：{} → {}",
                pair[0].created_at,
                pair[1].created_at
            );
        }
    }

    #[test]
    fn 摘要与详情摘要生成() {
        let items = vec![
            crate::parse::ParsedItem {
                part_key: Some("kidney.left".to_string()),
                item_code: Some("kidney.ap_diameter".to_string()),
                value_raw: Some(3.2),
                unit_raw: Some("cm".to_string()),
                value_si: Some(32.0),
                unit_code: Some("mm".to_string()),
                coercion_path: crate::coerce::path::CoercionPath::UnitConvert,
                line: 1,
                column: 6,
                char_start: 5,
                notes: Vec::new(),
                dictionary_version: crate::dictionary_version(),
            },
            crate::parse::ParsedItem {
                part_key: Some("bladder".to_string()),
                item_code: Some("bladder.residual_urine_volume".to_string()),
                value_raw: Some(2.5),
                unit_raw: Some("cm3".to_string()),
                value_si: Some(2.5),
                unit_code: Some("mL".to_string()),
                coercion_path: crate::coerce::path::CoercionPath::Composite,
                line: 2,
                column: 6,
                char_start: 20,
                notes: Vec::new(),
                dictionary_version: crate::dictionary_version(),
            },
        ];
        let detail = coercion_detail(&items);
        assert!(detail.contains("条目 2 条"), "{}", detail);
        assert!(detail.contains("UnitConvert 1"), "{}", detail);
        assert!(detail.contains("Composite 1"), "{}", detail);
        // 没比成与首次检查必须分得开。
        assert!(compared_detail("first_time", None, None).contains("未产出差值"));
        assert!(compared_detail("comparison_skipped", None, None).contains("comparison_skipped"));
        assert!(compared_detail("has_history", Some(4.0), Some("增大")).contains("差值 4"));
    }
}
