//! 状态仓储：报告行 / 结构化条目 / 对拍差值 / 审计留痕 / 幂等登记。
//!
//! **事务边界**：[`Repo::commit_ingestion`] 把「幂等查重 → 写报告 → 写条目 → 写审计」
//! 包在**同一个事务**里 —— 任一步失败整体回滚，**不落半条台账**。
//!
//! **幂等语义**：幂等键（`sha256(study_no‖报告内容)`）唯一约束冲突时返回
//! [`RepoError::AlreadyExists`]，**绝不静默覆盖**已有台账行。

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::parse::{Failure, ParsedItem};
use crate::state::schema;

/// 审计动作码的**唯一真源**在 [`crate::workflow::audit`] —— 这里只做转出，
/// 免得同一个动作在两处各写一份字符串（拼错一个字符，运维按码聚合就漏一类）。
pub use crate::workflow::audit::{
    ACTION_COERCED, ACTION_COMPARED, ACTION_CONFIRMED, ACTION_DUPLICATE_SKIPPED, ACTION_FAILED,
    ACTION_PARSED, ACTION_PUSHED, ACTION_RECEIVED, ACTION_REJECTED, ACTION_WRITTEN_BACK,
};

/// 仓储错误。
#[derive(Debug)]
pub enum RepoError {
    /// 幂等键（或唯一索引）冲突：这条已经处理过，**不覆盖**。
    AlreadyExists(String),
    /// 台账行不合法：数值与单位码不成对 / 无强转路径却带数值。
    ///
    /// **整笔回滚**（不落半条台账）—— 与源产品的「失败的申请不扣账」同一立场。
    InvalidItem(String),
    /// 记录不存在。
    NotFound(String),
    /// 文件系统错误（建库目录等）。
    Io(std::io::Error),
    /// 底层 SQLite 错误。
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for RepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoError::AlreadyExists(msg) => write!(f, "幂等键冲突：{}", msg),
            RepoError::InvalidItem(msg) => write!(f, "台账行不合法：{}", msg),
            RepoError::NotFound(msg) => write!(f, "记录不存在：{}", msg),
            RepoError::Io(e) => write!(f, "文件系统错误：{}", e),
            RepoError::Sqlite(e) => write!(f, "状态库错误：{}", e),
        }
    }
}

impl std::error::Error for RepoError {}

impl From<rusqlite::Error> for RepoError {
    fn from(e: rusqlite::Error) -> Self {
        RepoError::Sqlite(e)
    }
}

/// 判断是否为**唯一索引**冲突（区别于外键 / 非空等其它约束错误）。
fn is_unique_violation(err: &rusqlite::Error) -> bool {
    err.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation)
        && err.to_string().contains("UNIQUE constraint failed")
}

/// 唯一索引冲突时，指出**撞的是哪一列**（`reports.study_no` / `reports.idempotency_key`）。
///
/// 「同一份内容」与「同一个检查号」是**两回事**：前者是重复投递（幂等键命中），
/// 后者是同一检查号来了不同内容（检查号唯一索引拦下）。
/// 报错要说得出是哪一种 —— 不然运维只能看到一句「冲突」，无从判断要不要追。
fn unique_violation_column(err: &rusqlite::Error) -> Option<String> {
    if !is_unique_violation(err) {
        return None;
    }
    Some(
        err.to_string()
            .split("UNIQUE constraint failed: ")
            .nth(1)
            .unwrap_or("")
            .to_string(),
    )
}

/// 新报告行。
#[derive(Clone, Debug)]
pub struct NewReport {
    /// 检查号（跨系统匹配键）。
    pub study_no: String,
    /// 患者主索引（取不到为 `None`）。
    pub patient_id: Option<String>,
    /// 报告状态。
    pub status: String,
    /// 来源文件路径。
    pub source_path: Option<String>,
    /// 词典版本。
    pub dictionary_version: String,
    /// 幂等键。
    pub idempotency_key: String,
}

/// 报告行（读出）。
#[derive(Clone, Debug)]
pub struct ReportRow {
    /// 主键。
    pub id: i64,
    /// 检查号。
    pub study_no: String,
    /// 患者主索引。
    pub patient_id: Option<String>,
    /// 报告状态。
    pub status: String,
    /// 词典版本。
    pub dictionary_version: String,
    /// 幂等键。
    pub idempotency_key: String,
    /// 创建时间。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}

/// 新结构化条目行。
#[derive(Clone, Debug)]
pub struct NewItem {
    /// 所属报告。
    pub report_id: i64,
    /// 规范部位键。
    pub part_key: Option<String>,
    /// 测项码。
    pub item_code: Option<String>,
    /// 报告原值。
    pub value_raw: Option<f64>,
    /// 报告原单位。
    pub unit_raw: Option<String>,
    /// 归一值。
    pub value_si: Option<f64>,
    /// 归一单位码。
    pub unit_code: Option<String>,
    /// 强转路径。
    pub coercion_path: String,
    /// 1 基行号。
    pub line: usize,
    /// 1 基列号。
    pub column: usize,
    /// 1 基字符光标。
    pub char_start: usize,
    /// 备注（多条以 `;` 连接）。
    pub notes: String,
    /// 词典版本。
    pub dictionary_version: String,
}

/// 新对拍行。
#[derive(Clone, Debug)]
pub struct NewComparison {
    /// 所属报告。
    pub report_id: i64,
    /// 关联条目（可空）。
    pub item_id: Option<i64>,
    /// 规范部位键。
    pub part_key: Option<String>,
    /// 测项码。
    pub item_code: Option<String>,
    /// 历史值。
    pub previous_value: Option<f64>,
    /// 本次值。
    pub current_value: Option<f64>,
    /// 差值。
    pub delta: Option<f64>,
    /// 方向（增大 / 减小 / 持平）。
    pub direction: Option<String>,
    /// 单位码。
    pub unit_code: Option<String>,
    /// 对拍状态（has_history / first_time / ambiguous / comparison_skipped）。
    pub state: String,
}

/// **台账条目行**（读出）。
///
/// 与 [`NewItem`] 一一对应 —— 写进去的是哪些字段，读出来就是哪些字段，
/// 回写装配因此不需要「另一种形状」。
#[derive(Clone, Debug, PartialEq)]
pub struct ItemRow {
    /// 主键。
    pub id: i64,
    /// 规范部位键。
    pub part_key: Option<String>,
    /// 测项码。
    pub item_code: Option<String>,
    /// 报告原值。
    pub value_raw: Option<f64>,
    /// 报告原单位。
    pub unit_raw: Option<String>,
    /// 归一值。
    pub value_si: Option<f64>,
    /// 归一单位码。
    pub unit_code: Option<String>,
    /// 强转路径。
    pub coercion_path: String,
    /// 1 基行号。
    pub line: usize,
    /// 1 基列号。
    pub column: usize,
    /// 1 基字符光标。
    pub char_start: usize,
    /// 备注（多条以 `;` 连接）。
    pub notes: String,
    /// 词典版本。
    pub dictionary_version: String,
}

impl ItemRow {
    /// 台账行 → 解析条目（**只做形状转换，不重新解析**）。
    ///
    /// 值、单位码、强转路径一律取台账里那一份 ——
    /// 重新解析会得到另一套口径，回写装配就跟着漂了。
    ///
    /// **谁在用**：对拍入账（[`crate::compare::pipeline`]）要把台账行
    /// 送进对拍内核；回写装配（[`crate::outbound::review_api`]）要把台账行
    /// 送进外发报文 —— 两边都只读台账，不碰原文。
    pub fn to_parsed_item(&self) -> ParsedItem {
        ParsedItem {
            part_key: self.part_key.clone(),
            item_code: self.item_code.clone(),
            value_raw: self.value_raw,
            unit_raw: self.unit_raw.clone(),
            value_si: self.value_si,
            unit_code: self.unit_code.clone(),
            coercion_path: crate::coerce::path::CoercionPath::from_code(&self.coercion_path)
                .unwrap_or(crate::coerce::path::CoercionPath::None),
            line: self.line,
            column: self.column,
            char_start: self.char_start,
            notes: self
                .notes
                .split(';')
                .map(|note| note.trim())
                .filter(|note| !note.is_empty())
                .map(|note| note.to_string())
                .collect(),
            dictionary_version: self.dictionary_version.clone(),
        }
    }
}

/// **对拍差值行**（读出）。
#[derive(Clone, Debug, PartialEq)]
pub struct ComparisonRow {
    /// 主键。
    pub id: i64,
    /// 关联条目（可空）。
    pub item_id: Option<i64>,
    /// 规范部位键。
    pub part_key: Option<String>,
    /// 测项码。
    pub item_code: Option<String>,
    /// 历史值。
    pub previous_value: Option<f64>,
    /// 本次值。
    pub current_value: Option<f64>,
    /// 差值。
    pub delta: Option<f64>,
    /// 方向（增大 / 减小 / 持平）。
    pub direction: Option<String>,
    /// 单位码。
    pub unit_code: Option<String>,
    /// 对拍状态。
    pub state: String,
}

/// 审计条目。
#[derive(Clone, Debug)]
pub struct AuditEntry {
    /// 所属报告（可空）。
    pub report_id: Option<i64>,
    /// 检查号。
    pub study_no: Option<String>,
    /// 动作码。
    pub action: String,
    /// 责任人（系统动作记 `system`）。
    pub actor: Option<String>,
    /// 错误码（失败条目用）。
    pub error_code: Option<String>,
    /// 1 基行号。
    pub line: Option<usize>,
    /// 1 基列号。
    pub column: Option<usize>,
    /// 原文片段。
    pub raw_snippet: Option<String>,
    /// 输入摘要哈希（审计用，不做去重）。
    pub input_digest: Option<String>,
    /// 词典版本。
    pub dictionary_version: Option<String>,
    /// 详情。
    pub detail: Option<String>,
}

/// 幂等登记行（读出）。
#[derive(Clone, Debug)]
pub struct IdempotencyRow {
    /// 主键。
    pub id: i64,
    /// 幂等键。
    pub idempotency_key: String,
    /// 检查号。
    pub study_no: String,
    /// 登记用途（`ingest` / `writeback` 等）。
    pub scope: String,
    /// 登记状态。
    pub state: String,
    /// 登记时间。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}

/// 库级检查号锁行（读出）。
///
/// 锁**在库里**而不是在进程内存里 —— 守护进程与定时批处理同时跑时，
/// 「同一检查号不并发处理」才真的成立（进程重启后也成立）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StudyLockRow {
    /// 被锁住的检查号。
    pub study_no: String,
    /// 持有者标识（进程 / 线程 / 闸实例）。
    pub holder: String,
    /// 抢到时间。
    pub acquired_at: String,
    /// 租约到期时间（到期后可被接管）。
    pub expires_at: String,
}

/// 待处理事件（守护进程续跑用）。
#[derive(Clone, Debug)]
pub struct PendingEvent {
    /// 报告主键。
    pub report_id: i64,
    /// 检查号。
    pub study_no: String,
    /// 患者主索引。
    pub patient_id: Option<String>,
    /// 当前状态。
    pub status: String,
    /// 幂等键。
    pub idempotency_key: String,
    /// 更新时间。
    pub updated_at: String,
}

/// 一次入库的输入记录。
#[derive(Clone, Debug)]
pub struct IngestRecord<'a> {
    /// 检查号。
    pub study_no: &'a str,
    /// 患者主索引。
    pub patient_id: Option<&'a str>,
    /// 来源文件路径。
    pub source_path: Option<&'a str>,
    /// 幂等键。
    pub idempotency_key: &'a str,
    /// 词典版本（由词典自身版本号给出，不写死在代码里）。
    pub dictionary_version: String,
    /// 解析出的结构化条目。
    pub items: &'a [ParsedItem],
    /// 解析失败条目。
    pub failures: &'a [Failure],
}

/// 一次入库的结果。
#[derive(Clone, Debug)]
pub struct IngestResult {
    /// 报告主键。
    pub report_id: i64,
    /// 落库后的状态。
    pub status: String,
    /// 条目数。
    pub item_count: usize,
    /// 失败条目数。
    pub failure_count: usize,
}

/// **补处理的结果**：一份已入库报告被重算后的落点。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshResult {
    /// 报告主键（**与刷新前同一个** —— 不新增报告行）。
    pub report_id: i64,
    /// 刷新后的状态。
    pub status: String,
    /// 刷新后的条目数。
    pub item_count: usize,
    /// 刷新后的失败条目数。
    pub failure_count: usize,
    /// 刷新前有多少条台账行（被替换掉的那批）。
    pub replaced_items: usize,
}

/// **台账行合法性校验**：数值与单位码必须成对；无强转路径不得带数值。
///
/// **为什么这道闸必须存在**：台账行是「这个测量值是 mm 还是 mL」的**唯一凭据**。
/// 一个「有数值、没单位码」的行，读出来像是 32 mm，其实可能是 32 mL ——
/// 这种错落在台账里**看起来仍然像一个正常数字**，没人会发现。
/// 一个「无强转路径（`None`）却带着数值」的行更糟：它等于把「没换算成功」
/// 写成了「换算成功」，对拍时拿它去减历史值，差值就是编的。
///
/// **校验失败 ⇒ 整笔回滚**，不落半条台账 —— 与源产品
/// 「失败的申请不扣账」同一立场（`github_ref/pgrust/crates/_support/mcx/src/tests.rs`
/// 的 `failed reservation charged nothing`）。
///
/// 解析内核保证产出合法行；这道闸拦的是**外来写入**（手工构造的条目、
/// 将来新增的写入通道），是落库前的最后一道防线。
pub fn validate_ledger_item(item: &ParsedItem) -> Result<(), String> {
    use crate::coerce::path::CoercionPath;

    match (item.value_si.is_some(), item.unit_code.is_some()) {
        (true, true) => {}
        (false, false) => {}
        (true, false) => {
            return Err(format!(
                "数值 {:?} 有归一值但缺单位码（line={} column={}）",
                item.value_si, item.line, item.column
            ))
        }
        (false, true) => {
            return Err(format!(
                "单位码 {:?} 有单位但缺归一值（line={} column={}）",
                item.unit_code, item.line, item.column
            ))
        }
    }

    if item.coercion_path == CoercionPath::None && item.value_si.is_some() {
        return Err(format!(
            "强转路径为 None 却带着归一值 {:?}（line={} column={}）",
            item.value_si, item.line, item.column
        ));
    }

    Ok(())
}

/// 状态仓储。
pub struct Repo {
    conn: Connection,
}

impl Repo {
    /// 打开（必要时创建）状态库并完成迁移。
    pub fn open(path: &Path) -> Result<Self, RepoError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(RepoError::Io)?;
            }
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// 打开内存库（测试用）。
    pub fn open_in_memory() -> Result<Self, RepoError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    /// 接管一条已有连接并完成初始化。
    pub fn from_connection(conn: Connection) -> Result<Self, RepoError> {
        schema::init(&conn)?;
        Ok(Repo { conn })
    }

    /// 只读访问底层连接（导出与统计用）。
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// 写一条报告行；幂等键冲突返回 [`RepoError::AlreadyExists`]。
    pub fn insert_report(&self, report: &NewReport) -> Result<i64, RepoError> {
        let now = crate::state::now_datetime();
        let result = self.conn.execute(
            "INSERT INTO reports (study_no, patient_id, status, source_path, dictionary_version, \
             idempotency_key, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?7)",
            params![
                report.study_no,
                report.patient_id,
                report.status,
                report.source_path,
                report.dictionary_version,
                report.idempotency_key,
                now
            ],
        );
        match result {
            Ok(_) => Ok(self.conn.last_insert_rowid()),
            Err(e) => Err(map_write_error(e, "reports.idempotency_key")),
        }
    }

    /// 写一条结构化条目。
    pub fn insert_item(&self, item: &NewItem) -> Result<i64, RepoError> {
        let now = crate::state::now_datetime();
        let result = self.conn.execute(
            "INSERT INTO items (report_id, part_key, item_code, value_raw, unit_raw, value_si, \
             unit_code, coercion_path, line, column, char_start, notes, dictionary_version, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                item.report_id,
                item.part_key,
                item.item_code,
                item.value_raw,
                item.unit_raw,
                item.value_si,
                item.unit_code,
                item.coercion_path,
                item.line as i64,
                item.column as i64,
                item.char_start as i64,
                item.notes,
                item.dictionary_version,
                now
            ],
        );
        match result {
            Ok(_) => Ok(self.conn.last_insert_rowid()),
            Err(e) => Err(map_write_error(e, "items")),
        }
    }

    /// 写一条对拍差值。
    pub fn insert_comparison(&self, cmp: &NewComparison) -> Result<i64, RepoError> {
        let now = crate::state::now_datetime();
        let result = self.conn.execute(
            "INSERT INTO comparisons (report_id, item_id, part_key, item_code, previous_value, \
             current_value, delta, direction, unit_code, state, created_at, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)",
            params![
                cmp.report_id,
                cmp.item_id,
                cmp.part_key,
                cmp.item_code,
                cmp.previous_value,
                cmp.current_value,
                cmp.delta,
                cmp.direction,
                cmp.unit_code,
                cmp.state,
                now
            ],
        );
        match result {
            Ok(_) => Ok(self.conn.last_insert_rowid()),
            Err(e) => Err(map_write_error(e, "comparisons")),
        }
    }

    /// 追加一条审计留痕。
    ///
    /// 留痕时间用**毫秒精度**（`now_datetime_millis`）：一份报告的处理会在
    /// 同一秒内落下多条留痕，秒级时间戳分不清先后。
    /// 审计链的**权威顺序**是自增主键 `id`（严格递增）。
    pub fn append_audit(&self, entry: &AuditEntry) -> Result<i64, RepoError> {
        let now = crate::state::now_datetime_millis();
        let result = self.conn.execute(
            "INSERT INTO audit_log (report_id, study_no, action, actor, error_code, line, column, \
             raw_snippet, input_digest, dictionary_version, detail, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                entry.report_id,
                entry.study_no,
                entry.action,
                entry.actor,
                entry.error_code,
                entry.line.map(|v| v as i64),
                entry.column.map(|v| v as i64),
                entry.raw_snippet,
                entry.input_digest,
                entry.dictionary_version,
                entry.detail,
                now
            ],
        );
        match result {
            Ok(_) => Ok(self.conn.last_insert_rowid()),
            Err(e) => Err(map_write_error(e, "audit_log")),
        }
    }

    /// 按幂等键查报告行；命中即说明这条已经处理过。
    pub fn find_by_idempotency_key(&self, key: &str) -> Result<Option<ReportRow>, RepoError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, study_no, patient_id, status, dictionary_version, idempotency_key, \
             created_at, updated_at FROM reports WHERE idempotency_key = ?1",
        )?;
        let row = stmt
            .query_row(params![key], |row| {
                Ok(ReportRow {
                    id: row.get(0)?,
                    study_no: row.get(1)?,
                    patient_id: row.get(2)?,
                    status: row.get(3)?,
                    dictionary_version: row.get(4)?,
                    idempotency_key: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// 按**检查号**查报告行；命中即说明这个检查号已经落过库。
    ///
    /// 幂等键挡的是「同一份内容」；这条挡的是「**同一个检查号**」——
    /// 两者不是一回事：工作站改过又重导，幂等键会变，但检查号不变。
    /// 库级唯一索引（`idx_reports_study_no`）是最终兜底，这条查的是**它拦下之后**，
    /// 已有报告是哪一份（留痕要指得准）。
    pub fn find_by_study_no(&self, study_no: &str) -> Result<Option<ReportRow>, RepoError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, study_no, patient_id, status, dictionary_version, idempotency_key, \
             created_at, updated_at FROM reports WHERE study_no = ?1",
        )?;
        let row = stmt
            .query_row(params![study_no], |row| {
                Ok(ReportRow {
                    id: row.get(0)?,
                    study_no: row.get(1)?,
                    patient_id: row.get(2)?,
                    status: row.get(3)?,
                    dictionary_version: row.get(4)?,
                    idempotency_key: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// 按**报告主键**查报告行。
    ///
    /// 对拍那一步已经握着刚落库的主键（不用拿检查号反查 ——
    /// HL7 通道的检查号取自 `OBR-3`，与投递目录名可能不同）。
    pub fn find_by_id(&self, report_id: i64) -> Result<Option<ReportRow>, RepoError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, study_no, patient_id, status, dictionary_version, idempotency_key, \
             created_at, updated_at FROM reports WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![report_id], |row| {
                Ok(ReportRow {
                    id: row.get(0)?,
                    study_no: row.get(1)?,
                    patient_id: row.get(2)?,
                    status: row.get(3)?,
                    dictionary_version: row.get(4)?,
                    idempotency_key: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// **抢一把库级检查号锁**（不等待）。
    ///
    /// - 没人占 → 落一行锁，返回 `true`（抢到了）。
    /// - 有人占且**未过期** → 返回 `false`（抢不到）。
    /// - 有人占但**已过期**（持锁进程崩了）→ 接管它，返回 `true`。
    ///
    /// **过期才能接管**：否则一个挂死的进程会把检查号永久卡死，
    /// 那份报告就再也进不了台账 —— 比多落一行更难解释。
    ///
    /// **跨进程成立**：锁在库里，不在进程内存里 ——
    /// 守护进程与定时批处理同时跑时也只串得住一把。
    pub fn try_acquire_study_lock(
        &self,
        study_no: &str,
        holder: &str,
        expires_at: &str,
    ) -> Result<bool, RepoError> {
        let now = crate::state::now_datetime();
        let inserted = self.conn.execute(
            "INSERT INTO study_locks (study_no, holder, acquired_at, expires_at) \
             VALUES (?1,?2,?3,?4)",
            params![study_no, holder, now, expires_at],
        );
        match inserted {
            Ok(_) => Ok(true),
            Err(e) if is_unique_violation(&e) => {
                // 已被占：只有**过期**的锁能接管。
                let taken = self.conn.execute(
                    "UPDATE study_locks SET holder=?2, acquired_at=?3, expires_at=?4 \
                     WHERE study_no=?1 AND expires_at <= ?3",
                    params![study_no, holder, now, expires_at],
                )?;
                Ok(taken > 0)
            }
            Err(e) => Err(RepoError::Sqlite(e)),
        }
    }

    /// **释放**一把库级检查号锁（持有者对得上才放得掉）。
    ///
    /// 持有者对不上时返回 `false` 且**不动别人的锁** ——
    /// 租约到期后被别人接管了，这时旧持有者醒来不能把新持有者的锁删掉。
    pub fn release_study_lock(&self, study_no: &str, holder: &str) -> Result<bool, RepoError> {
        let affected = self.conn.execute(
            "DELETE FROM study_locks WHERE study_no = ?1 AND holder = ?2",
            params![study_no, holder],
        )?;
        Ok(affected > 0)
    }

    /// 查一把库级检查号锁现在归谁（`None` = 没人占）。
    pub fn study_lock_of(&self, study_no: &str) -> Result<Option<StudyLockRow>, RepoError> {
        let mut stmt = self.conn.prepare(
            "SELECT study_no, holder, acquired_at, expires_at FROM study_locks WHERE study_no = ?1",
        )?;
        let row = stmt
            .query_row(params![study_no], |row| {
                Ok(StudyLockRow {
                    study_no: row.get(0)?,
                    holder: row.get(1)?,
                    acquired_at: row.get(2)?,
                    expires_at: row.get(3)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// 库里现存的锁行数（运维自检 / 测试用）。
    pub fn count_study_locks(&self) -> Result<i64, RepoError> {
        let count = self
            .conn
            .query_row("SELECT COUNT(*) FROM study_locks", [], |row| row.get(0))?;
        Ok(count)
    }

    /// 按幂等键查**幂等登记表**；命中即说明这条已经登记过。
    ///
    /// 取件前的幂等预查走这条路径：登记表的唯一索引是重复投递的最终兜底
    /// （进程重启后依然成立，不依赖进程内内存锁）。
    ///
    /// **不限用途**：同一个键可能在 `ingest` 与 `writeback` 两个用途下各登记一次
    /// （唯一索引是 **(键, 用途)** 组合，见 [`crate::state::schema`]）。
    /// 要按用途精确查，走 [`Repo::find_idempotency_scoped`]。
    pub fn find_idempotency(&self, key: &str) -> Result<Option<IdempotencyRow>, RepoError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, idempotency_key, study_no, scope, state, created_at, updated_at \
             FROM idempotency WHERE idempotency_key = ?1 ORDER BY id LIMIT 1",
        )?;
        let row = stmt
            .query_row(params![key], |row| {
                Ok(IdempotencyRow {
                    id: row.get(0)?,
                    idempotency_key: row.get(1)?,
                    study_no: row.get(2)?,
                    scope: row.get(3)?,
                    state: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// 按 **(幂等键, 登记用途)** 精确查幂等登记行。
    ///
    /// 用途分开查，才分得清「这份内容没入库过」与「入库了但还没回写过」——
    /// 两个用途共用一把钥匙，只有组合索引才表达得了。
    pub fn find_idempotency_scoped(
        &self,
        key: &str,
        scope: &str,
    ) -> Result<Option<IdempotencyRow>, RepoError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, idempotency_key, study_no, scope, state, created_at, updated_at \
             FROM idempotency WHERE idempotency_key = ?1 AND scope = ?2",
        )?;
        let row = stmt
            .query_row(params![key, scope], |row| {
                Ok(IdempotencyRow {
                    id: row.get(0)?,
                    idempotency_key: row.get(1)?,
                    study_no: row.get(2)?,
                    scope: row.get(3)?,
                    state: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// 某份报告已落库的结构化条目数（幂等命中时用来报「这份本来有几条」）。
    pub fn count_items_of_report(&self, report_id: i64) -> Result<i64, RepoError> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM items WHERE report_id = ?1",
            params![report_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// **推进报告状态**（状态机守卫生效后的落库出口）。
    ///
    /// **只改状态，不碰别的** —— 条目与差值在落库那一刻就定下来了，
    /// 状态推进改不了结论，只能改「这份结论走到哪一步了」。
    ///
    /// 返回 `false` 表示**没有这一行**（报告不存在）——
    /// 调用方据此区分「状态没推进成功」与「报告根本不存在」，
    /// 不把两者混成一句「更新失败」。
    pub fn set_report_status(&self, report_id: i64, status: &str) -> Result<bool, RepoError> {
        let now = crate::state::now_datetime();
        let changed = self.conn.execute(
            "UPDATE reports SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![report_id, status, now],
        )?;
        Ok(changed > 0)
    }

    /// 读回一份报告已落库的结构化条目（回写装配用）。
    ///
    /// **读回来的就是台账里的那份** —— 回写报文由它装配，
    /// 不重新解析原文（重新解析会得到另一套口径，
    /// 而医师确认的恰恰是台账里那一套）。
    pub fn items_of_report(&self, report_id: i64) -> Result<Vec<ItemRow>, RepoError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, part_key, item_code, value_raw, unit_raw, value_si, unit_code, \
             coercion_path, line, column, char_start, notes, dictionary_version \
             FROM items WHERE report_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![report_id], |row| {
            Ok(ItemRow {
                id: row.get(0)?,
                part_key: row.get(1)?,
                item_code: row.get(2)?,
                value_raw: row.get(3)?,
                unit_raw: row.get(4)?,
                value_si: row.get(5)?,
                unit_code: row.get(6)?,
                coercion_path: row.get(7)?,
                line: row.get::<_, i64>(8)? as usize,
                column: row.get::<_, i64>(9)? as usize,
                char_start: row.get::<_, i64>(10)? as usize,
                notes: row.get(11)?,
                dictionary_version: row.get(12)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 读回一份报告已落库的对拍差值（回写装配用）。
    pub fn comparisons_of_report(&self, report_id: i64) -> Result<Vec<ComparisonRow>, RepoError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, item_id, part_key, item_code, previous_value, current_value, delta, \
             direction, unit_code, state FROM comparisons WHERE report_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![report_id], |row| {
            Ok(ComparisonRow {
                id: row.get(0)?,
                item_id: row.get(1)?,
                part_key: row.get(2)?,
                item_code: row.get(3)?,
                previous_value: row.get(4)?,
                current_value: row.get(5)?,
                delta: row.get(6)?,
                direction: row.get(7)?,
                unit_code: row.get(8)?,
                state: row.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 某份报告已落库的失败条目数（推送摘要里的「待处理失败条目」）。
    pub fn failure_count_of_report(&self, report_id: i64) -> Result<i64, RepoError> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM audit_log WHERE report_id = ?1 AND action = ?2",
            params![report_id, crate::workflow::audit::ACTION_FAILED],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// 某份报告已落库的**失败条目**（读出来）。
    ///
    /// 失败条目的真源是 `audit_log` 里 `failed` 动作的留痕 ——
    /// 入库事务逐条写下的**稳定错误码 + 行列 + 原文片段**。
    ///
    /// **码表里没有的码跳过**（不猜）：那说明口径漂了，
    /// 把它硬塞进一个已知码里，运维按码聚合就统计错了。
    pub fn failures_of_report(&self, report_id: i64) -> Result<Vec<Failure>, RepoError> {
        let mut stmt = self.conn.prepare(
            "SELECT error_code, line, column, raw_snippet FROM audit_log \
             WHERE report_id = ?1 AND action = ?2 ORDER BY id",
            )?;
        let rows = stmt.query_map(params![report_id, crate::workflow::audit::ACTION_FAILED], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut failures = Vec::new();
        for row in rows {
            let (code, line, column, snippet) = row?;
            let code = match code.as_deref().and_then(crate::parse::ErrorCode::lookup) {
                Some(code) => code,
                None => continue,
            };
            failures.push(Failure::new(
                code,
                0,
                line.unwrap_or(0) as usize,
                column.unwrap_or(0) as usize,
                snippet.as_deref().unwrap_or(""),
            ));
        }
        Ok(failures)
    }

    /// **登记一次回写**（幂等键 + 用途 `writeback`）。
    ///
    /// 返回 `true` = 这次是**首次**登记（可以发写请求）；
    /// `false` = 同一把钥匙在 `writeback` 用途下已经登记过
    /// —— **不再发第二次写请求**（重复确认不产生第二次回写）。
    ///
    /// 幂等表的唯一索引是 **(键, 用途)** 组合 ——
    /// 入库与回写各登记一次是合法的（同一把钥匙、两个用途）。
    pub fn claim_writeback(
        &self,
        idempotency_key: &str,
        study_no: &str,
    ) -> Result<bool, RepoError> {
        let now = crate::state::now_datetime();
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO idempotency (idempotency_key, study_no, scope, state, created_at, updated_at) \
             VALUES (?1,?2,?3,'done',?4,?4)",
            params![
                idempotency_key,
                study_no,
                crate::workflow::idempotency::SCOPE_WRITEBACK,
                now
            ],
        )?;
        Ok(changed > 0)
    }

    /// **释放一次回写登记**（回写失败时用）。
    ///
    /// 写失败要把这把钥匙让出来 —— 否则重试通道再也登记不上，
    /// 一次失败就把这份报告永久挡在重试之外。
    pub fn release_writeback_claim(&self, idempotency_key: &str) -> Result<bool, RepoError> {
        let removed = self.conn.execute(
            "DELETE FROM idempotency WHERE idempotency_key = ?1 AND scope = ?2",
            params![
                idempotency_key,
                crate::workflow::idempotency::SCOPE_WRITEBACK
            ],
        )?;
        Ok(removed > 0)
    }

    /// 某个用途下**已受理的写请求数**（幂等登记表行数）。
    ///
    /// 这是「写请求数」的**可核对口径**：每一笔真正落库的写入都在这里登记一行，
    /// 重复投递被拦下时**不登记**（登记在事务里，随回滚一起撤销）。
    pub fn count_accepted_writes(&self, scope: &str) -> Result<i64, RepoError> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM idempotency WHERE scope = ?1",
            params![scope],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// **被幂等拦下的写请求数**（`duplicate_skipped` 留痕条数）。
    ///
    /// 与 [`Repo::count_accepted_writes`] 对照就是运维要的那句话：
    /// 「今天受理了 N 份，另有 M 份因为重复投递被跳过」。
    pub fn count_skipped_writes(&self) -> Result<i64, RepoError> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = ?1",
            params![crate::workflow::audit::ACTION_DUPLICATE_SKIPPED],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// 列出待处理事件（守护进程重启后续跑用）。
    ///
    /// 状态词表在 Task 33（状态机）正式定稿；此处取「已入库但未走完流程」的报告。
    pub fn claim_pending_events(&self, limit: usize) -> Result<Vec<PendingEvent>, RepoError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, study_no, patient_id, status, idempotency_key, updated_at FROM reports \
             WHERE status IN ('received','parsed','awaiting_review','pending_writeback') \
             ORDER BY updated_at, id LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(PendingEvent {
                report_id: row.get(0)?,
                study_no: row.get(1)?,
                patient_id: row.get(2)?,
                status: row.get(3)?,
                idempotency_key: row.get(4)?,
                updated_at: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 按状态计数（守护心跳与运行摘要用）。
    ///
    /// 回答运维那个问题：「昨天有多少报告没解析出来」—— 状态维度的分布一眼可见。
    pub fn count_by_status(&self) -> Result<Vec<(String, i64)>, RepoError> {
        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) FROM reports GROUP BY status ORDER BY status")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 最近一条审计事件的时间（心跳的「最近事件时间」；还没有事件时为 `None`）。
    pub fn last_event_at(&self) -> Result<Option<String>, RepoError> {
        let value = self.conn.query_row(
            "SELECT MAX(created_at) FROM audit_log",
            [],
            |row| row.get::<_, Option<String>>(0),
        )?;
        Ok(value)
    }

    /// **一次入库的完整事务**：幂等查重 → 写报告 → 写条目 → 写审计。
    ///
    /// - 幂等键命中已有报告 → 返回 [`RepoError::AlreadyExists`]，**不产生第二条台账行**。
    /// - 任一步失败 → 整个事务回滚，**不落半条台账**。
    ///
    /// **为什么查重放在事务里**（而不是事务外先查一遍）：
    /// 事务外的查重是「先问后做」——两个并发投递可能**同时**查不到、都以为自己是首次，
    /// 然后都去写。真正的兜底是数据库唯一索引，而唯一索引只有在**写**的时候才会被触发。
    /// 所以查重必须与写**同处一个事务**，由索引说话。
    ///
    /// **为什么第一步就是写**（幂等登记的 `INSERT` 而不是 `SELECT`）：
    /// SQLite 的延迟事务在「先读后写」时可能撞上写锁升级的死锁（两个事务都在读，
    /// 都在写时互相等，busy handler 也救不了）。让事务的**第一条语句就是写**，
    /// 写锁在事务开头就拿到，升级路径根本不存在。
    pub fn commit_ingestion(&self, rec: &IngestRecord) -> Result<IngestResult, RepoError> {
        let status = if !rec.items.is_empty() {
            "parsed"
        } else if !rec.failures.is_empty() {
            "parse_failed"
        } else {
            "received"
        };

        let tx = self.conn.unchecked_transaction()?;

        // 1) 幂等登记（**事务里的第一条语句就是写**）：唯一索引冲突即整体回滚
        //    （并发重投在此拦下 —— 这是最终兜底，不依赖进程内内存锁）。
        if let Err(e) = tx.execute(
            "INSERT INTO idempotency (idempotency_key, study_no, scope, state, created_at, updated_at) \
             VALUES (?1,?2,?3,'done',?4,?4)",
            params![
                rec.idempotency_key,
                rec.study_no,
                crate::workflow::idempotency::SCOPE_INGEST,
                crate::state::now_datetime()
            ],
        ) {
            if is_unique_violation(&e) {
                return Err(RepoError::AlreadyExists(format!(
                    "study_no={} 的报告已入库，幂等键重复",
                    rec.study_no
                )));
            }
            return Err(RepoError::Sqlite(e));
        }

        // 2) 幂等查重（**在事务里**）：命中即跳过，不产生第二条台账行。
        //    上面那步的索引冲突通常已经拦下了；这一步兜住
        //    「报告行在、登记行不在」这种数据不一致的边界情况。
        let existed: Option<i64> = tx
            .query_row(
                "SELECT id FROM reports WHERE idempotency_key = ?1",
                params![rec.idempotency_key],
                |row| row.get(0),
            )
            .optional()?;
        if existed.is_some() {
            return Err(RepoError::AlreadyExists(format!(
                "study_no={} 的报告已入库，幂等键重复",
                rec.study_no
            )));
        }

        // 3) 写报告行。
        let inserted = tx.execute(
            "INSERT INTO reports (study_no, patient_id, status, source_path, dictionary_version, \
             idempotency_key, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?7)",
            params![
                rec.study_no,
                rec.patient_id,
                status,
                rec.source_path,
                rec.dictionary_version,
                rec.idempotency_key,
                crate::state::now_datetime()
            ],
        );
        let report_id = match inserted {
            Ok(_) => tx.last_insert_rowid(),
            Err(e) => {
                if is_unique_violation(&e) {
                    // 撞的是**检查号**还是**幂等键**，说出来不一样：
                    // 前者是「同一检查号来了不同内容」，后者是「同一份内容重复投递」。
                    let column = unique_violation_column(&e).unwrap_or_default();
                    return Err(RepoError::AlreadyExists(if column.contains("study_no") {
                        format!("study_no={} 已有报告入库（检查号唯一约束）", rec.study_no)
                    } else {
                        format!("study_no={} 的报告已入库，幂等键重复", rec.study_no)
                    }));
                }
                return Err(RepoError::Sqlite(e));
            }
        };

        // 4) 写结构化条目（台账行的来源）。
        //
        //    **逐条校验**：不合法就整笔回滚 —— 台账里不能出现
        //    「有数值没单位」或「没换算成功却带数值」的行。
        let now = crate::state::now_datetime();
        for (idx, item) in rec.items.iter().enumerate() {
            if let Err(reason) = validate_ledger_item(item) {
                // `tx` 在这里被丢弃 ⇒ 回滚：报告行、条目、审计、幂等登记**全部撤销**，
                // 不落半条台账（与源产品「失败的申请不扣账」同一立场）。
                return Err(RepoError::InvalidItem(format!(
                    "study_no={} 第 {} 条：{}",
                    rec.study_no,
                    idx + 1,
                    reason
                )));
            }
            tx.execute(
                "INSERT INTO items (report_id, part_key, item_code, value_raw, unit_raw, value_si, \
                 unit_code, coercion_path, line, column, char_start, notes, dictionary_version, created_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    report_id,
                    item.part_key,
                    item.item_code,
                    item.value_raw,
                    item.unit_raw,
                    item.value_si,
                    item.unit_code,
                    item.coercion_path.as_str(),
                    item.line as i64,
                    item.column as i64,
                    item.char_start as i64,
                    item.notes.join(";"),
                    // 台账行的版本 = **产出这条条目时生效的词典版本**。
                    // 调用方盖的版本只作兜底（条目没带版本时）——
                    // 口径留痕必须说的是「这条数据是哪套口径算出来的」，
                    // 而不是「落库那一刻词典是哪个版本」。
                    item.dictionary_version_if_empty(&rec.dictionary_version),
                    now
                ],
            )?;
        }

        // 5) 写审计：接收 / 解析 / 强转 / 失败（失败条目逐条留痕，带错误码与行列）。
        //
        // **动作码来自 `crate::workflow::audit`**（唯一真源）——
        // 这里只写「哪几步发生了」，审计层负责「怎么记」。
        // 留痕与台账行在**同一个事务**里：要么都落，要么都不落
        // （落了台账却没留痕 = 半年后内审查不到这份数据怎么来的）。
        let digest = rec.idempotency_key.to_string();
        let audit_now = crate::state::now_datetime_millis();
        tx.execute(
            "INSERT INTO audit_log (report_id, study_no, action, actor, error_code, line, column, \
             raw_snippet, input_digest, dictionary_version, detail, created_at) \
             VALUES (?1,?2,?3,'system',NULL,NULL,NULL,NULL,?4,?5,?6,?7)",
            params![
                report_id,
                rec.study_no,
                ACTION_RECEIVED,
                digest,
                rec.dictionary_version,
                format!("接收报告：{} 份", 1),
                audit_now
            ],
        )?;
        tx.execute(
            "INSERT INTO audit_log (report_id, study_no, action, actor, error_code, line, column, \
             raw_snippet, input_digest, dictionary_version, detail, created_at) \
             VALUES (?1,?2,?3,'system',NULL,NULL,NULL,NULL,?4,?5,?6,?7)",
            params![
                report_id,
                rec.study_no,
                ACTION_PARSED,
                digest,
                rec.dictionary_version,
                format!("解析完成：条目 {} 条，失败 {} 条", rec.items.len(), rec.failures.len()),
                audit_now
            ],
        )?;
        // 强转留痕：口径落定的那一步（哪几条走了换算、哪几条没路）。
        tx.execute(
            "INSERT INTO audit_log (report_id, study_no, action, actor, error_code, line, column, \
             raw_snippet, input_digest, dictionary_version, detail, created_at) \
             VALUES (?1,?2,?3,'system',NULL,NULL,NULL,NULL,?4,?5,?6,?7)",
            params![
                report_id,
                rec.study_no,
                ACTION_COERCED,
                digest,
                rec.dictionary_version,
                crate::workflow::audit::coercion_detail(rec.items),
                audit_now
            ],
        )?;
        for failure in rec.failures {
            tx.execute(
                "INSERT INTO audit_log (report_id, study_no, action, actor, error_code, line, column, \
                 raw_snippet, input_digest, dictionary_version, detail, created_at) \
                 VALUES (?1,?2,?3,'system',?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    report_id,
                    rec.study_no,
                    ACTION_FAILED,
                    failure.code.as_str(),
                    failure.line as i64,
                    failure.column as i64,
                    failure.raw_snippet,
                    digest,
                    rec.dictionary_version,
                    failure.message(),
                    audit_now
                ],
            )?;
        }

        tx.commit()?;

        Ok(IngestResult {
            report_id,
            status: status.to_string(),
            item_count: rec.items.len(),
            failure_count: rec.failures.len(),
        })
    }

    /// **补处理**：把一份已入库报告的台账行**重算**一遍。
    ///
    /// 与 [`Repo::commit_ingestion`] 的分工：
    ///
    /// | | 触发场景 | 报告行 | 台账行 |
    /// |---|---|---|---|
    /// | `commit_ingestion` | **首次受理**（新投递） | 新增 1 行 | 新增 |
    /// | `refresh_ingestion` | **补处理**（词典改过 / 上一版解析有缺口） | **原样 1 行** | **替换** |
    ///
    /// **为什么要「替换」而不是「再追加」**：台账行是**按口径算出来的**。
    /// 词典改过之后，同一份报告重新解析的结果与库里旧行不同 ——
    /// 旧行是**旧口径**的产物；追加一份新的，台账就变成一半旧口径一半新口径，
    /// 对账时说不清「这一行是哪套口径算出来的」。
    ///
    /// **不产生第二条台账行**：报告行不新增，条目是**替换**；
    /// 失败条目（`audit_log` 里 `failed` 动作的留痕）也一并替换 ——
    /// 留着旧的失败留痕会让「失败条目数」翻倍，运维按错误码聚合就全错了。
    ///
    /// **审计链只增不删**：接收 / 解析 / 强转的旧留痕保留，
    /// 补处理再追加一段新留痕（`detail` 写明「补处理」）——
    /// 「这份数据什么时候、按哪套口径算出来的」要说得清。
    ///
    /// **事务边界**：删旧行与落新行在**同一个事务**里 ——
    /// 中途失败则整体回滚，台账回到补处理前的样子（不落半份台账）。
    pub fn refresh_ingestion(
        &self,
        rec: &IngestRecord,
        report_id: i64,
    ) -> Result<RefreshResult, RepoError> {
        let status = if !rec.items.is_empty() {
            "parsed"
        } else if !rec.failures.is_empty() {
            "parse_failed"
        } else {
            "received"
        };

        let tx = self.conn.unchecked_transaction()?;

        // 补处理前先确认这份报告**确实在库里**（不凭空刷新一个不存在的报告）。
        let exists: Option<i64> = tx
            .query_row(
                "SELECT id FROM reports WHERE id = ?1",
                params![report_id],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(RepoError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
        }

        // 旧台账行数（替换掉的那批）。
        let replaced_items: i64 = tx.query_row(
            "SELECT COUNT(*) FROM items WHERE report_id = ?1",
            params![report_id],
            |row| row.get(0),
        )?;

        // 1) 删旧台账行（**替换**，不是追加）。
        tx.execute(
            "DELETE FROM items WHERE report_id = ?1",
            params![report_id],
        )?;

        // 2) 删旧的失败留痕：失败条目随重算结果一起换掉，
        //    否则「失败条目数」翻倍，运维按错误码聚合就全错了。
        tx.execute(
            "DELETE FROM audit_log WHERE report_id = ?1 AND action = ?2",
            params![report_id, ACTION_FAILED],
        )?;

        // 3) 报告行：状态与词典版本跟着重算结果走（报告行本身不新增）。
        tx.execute(
            "UPDATE reports SET status = ?1, dictionary_version = ?2, updated_at = ?3 WHERE id = ?4",
            params![
                status,
                rec.dictionary_version,
                crate::state::now_datetime(),
                report_id
            ],
        )?;

        // 4) 落新台账行（逐条校验；不合法就整笔回滚 —— 不落半份台账）。
        let now = crate::state::now_datetime();
        for (idx, item) in rec.items.iter().enumerate() {
            if let Err(reason) = validate_ledger_item(item) {
                return Err(RepoError::InvalidItem(format!(
                    "study_no={} 第 {} 条：{}",
                    rec.study_no,
                    idx + 1,
                    reason
                )));
            }
            tx.execute(
                "INSERT INTO items (report_id, part_key, item_code, value_raw, unit_raw, value_si, \
                 unit_code, coercion_path, line, column, char_start, notes, dictionary_version, created_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    report_id,
                    item.part_key,
                    item.item_code,
                    item.value_raw,
                    item.unit_raw,
                    item.value_si,
                    item.unit_code,
                    item.coercion_path.as_str(),
                    item.line as i64,
                    item.column as i64,
                    item.char_start as i64,
                    item.notes.join(";"),
                    item.dictionary_version_if_empty(&rec.dictionary_version),
                    now
                ],
            )?;
        }

        // 5) 补处理的留痕（追加，不删旧链）。
        let digest = rec.idempotency_key.to_string();
        let audit_now = crate::state::now_datetime_millis();
        tx.execute(
            "INSERT INTO audit_log (report_id, study_no, action, actor, error_code, line, column, \
             raw_snippet, input_digest, dictionary_version, detail, created_at) \
             VALUES (?1,?2,?3,'system',NULL,NULL,NULL,NULL,?4,?5,?6,?7)",
            params![
                report_id,
                rec.study_no,
                ACTION_PARSED,
                digest,
                rec.dictionary_version,
                format!(
                    "补处理重算：条目 {} 条，失败 {} 条（替换旧台账行 {} 条）",
                    rec.items.len(),
                    rec.failures.len(),
                    replaced_items
                ),
                audit_now
            ],
        )?;
        tx.execute(
            "INSERT INTO audit_log (report_id, study_no, action, actor, error_code, line, column, \
             raw_snippet, input_digest, dictionary_version, detail, created_at) \
             VALUES (?1,?2,?3,'system',NULL,NULL,NULL,NULL,?4,?5,?6,?7)",
            params![
                report_id,
                rec.study_no,
                ACTION_COERCED,
                digest,
                rec.dictionary_version,
                crate::workflow::audit::coercion_detail(rec.items),
                audit_now
            ],
        )?;
        for failure in rec.failures {
            tx.execute(
                "INSERT INTO audit_log (report_id, study_no, action, actor, error_code, line, column, \
                 raw_snippet, input_digest, dictionary_version, detail, created_at) \
                 VALUES (?1,?2,?3,'system',?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    report_id,
                    rec.study_no,
                    ACTION_FAILED,
                    failure.code.as_str(),
                    failure.line as i64,
                    failure.column as i64,
                    failure.raw_snippet,
                    digest,
                    rec.dictionary_version,
                    failure.message(),
                    audit_now
                ],
            )?;
        }

        tx.commit()?;

        Ok(RefreshResult {
            report_id,
            status: status.to_string(),
            item_count: rec.items.len(),
            failure_count: rec.failures.len(),
            replaced_items: replaced_items as usize,
        })
    }
}

/// 写操作的错误映射：唯一索引冲突 → `AlreadyExists`，其余 → `Sqlite`。
fn map_write_error(err: rusqlite::Error, target: &str) -> RepoError {
    if is_unique_violation(&err) {
        RepoError::AlreadyExists(format!("{} 唯一约束冲突", target))
    } else {
        RepoError::Sqlite(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_report;

    fn key(study_no: &str, body: &str) -> String {
        crate::state::idempotency_key(study_no, body)
    }

    #[test]
    fn 入库写出一条报告行与一条条目() {
        let repo = Repo::open_in_memory().unwrap();
        let outcome = parse_report("左肾积水 3.2cm");
        let body = "左肾积水 3.2cm";
        let res = repo
            .commit_ingestion(&IngestRecord {
                study_no: "SAMPLE-A",
                patient_id: Some("PID-EXAMPLE"),
                source_path: Some("inbox/SAMPLE-A/report.txt"),
                idempotency_key: &key("SAMPLE-A", body),
                dictionary_version: crate::dictionary_version(),
                items: &outcome.items,
                failures: &outcome.failures,
            })
            .unwrap();
        assert_eq!(res.item_count, 1);
        assert_eq!(res.status, "parsed");

        let found = repo.find_by_idempotency_key(&key("SAMPLE-A", body)).unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.study_no, "SAMPLE-A");
        assert_eq!(found.patient_id.as_deref(), Some("PID-EXAMPLE"));
    }

    #[test]
    fn 重复入库返回已存在且不产生第二条台账行() {
        let repo = Repo::open_in_memory().unwrap();
        let body = "左肾积水 3.2cm";
        let outcome = parse_report(body);
        let rec = IngestRecord {
            study_no: "SAMPLE-A",
            patient_id: None,
            source_path: None,
            idempotency_key: &key("SAMPLE-A", body),
            dictionary_version: crate::dictionary_version(),
            items: &outcome.items,
            failures: &outcome.failures,
        };
        repo.commit_ingestion(&rec).unwrap();
        let second = repo.commit_ingestion(&rec);
        assert!(matches!(second, Err(RepoError::AlreadyExists(_))));

        let count: i64 = repo
            .connection()
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "重复投递不得产生第二条台账行");
    }

    #[test]
    fn 失败条目带错误码与行列落审计表() {
        let repo = Repo::open_in_memory().unwrap();
        let body = "左肾积水 3.2";
        let outcome = parse_report(body);
        assert_eq!(outcome.failures.len(), 1);
        repo.commit_ingestion(&IngestRecord {
            study_no: "SAMPLE-B",
            patient_id: None,
            source_path: None,
            idempotency_key: &key("SAMPLE-B", body),
            dictionary_version: crate::dictionary_version(),
            items: &outcome.items,
            failures: &outcome.failures,
        })
        .unwrap();

        let (code, line, column): (String, i64, i64) = repo
            .connection()
            .query_row(
                "SELECT error_code, line, column FROM audit_log WHERE action = 'failed'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(code, "UNIT_MISSING");
        assert!(line >= 1);
        assert!(column >= 1);
    }

    #[test]
    fn 待处理事件可列出() {
        let repo = Repo::open_in_memory().unwrap();
        let body = "左肾积水 3.2cm";
        let outcome = parse_report(body);
        repo.commit_ingestion(&IngestRecord {
            study_no: "SAMPLE-C",
            patient_id: None,
            source_path: None,
            idempotency_key: &key("SAMPLE-C", body),
            dictionary_version: crate::dictionary_version(),
            items: &outcome.items,
            failures: &outcome.failures,
        })
        .unwrap();
        let pending = repo.claim_pending_events(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].study_no, "SAMPLE-C");
    }

    #[test]
    fn 写失败时整体回滚不落半条台账() {
        let repo = Repo::open_in_memory().unwrap();
        // 先塞一条非法条目（report_id 指向不存在的报告），触发外键失败。
        let bad = NewItem {
            report_id: 9999,
            part_key: Some("kidney.left".to_string()),
            item_code: Some("kidney.length".to_string()),
            value_raw: Some(3.2),
            unit_raw: Some("cm".to_string()),
            value_si: Some(32.0),
            unit_code: Some("mm".to_string()),
            coercion_path: "UnitConvert".to_string(),
            line: 1,
            column: 1,
            char_start: 1,
            notes: String::new(),
            dictionary_version: crate::dictionary_version(),
        };
        assert!(repo.insert_item(&bad).is_err());

        let reports: i64 = repo
            .connection()
            .query_row("SELECT COUNT(*) FROM reports", [], |row| row.get(0))
            .unwrap();
        assert_eq!(reports, 0);
    }
}
