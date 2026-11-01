//! **离线重放通道**（`replay`）：按幂等键把指定目录的报告**重新走一遍**。
//!
//! ## 这条通道解决什么
//!
//! 运维改了部位 / 测项词典之后，库里那些**按旧口径算出来的**台账行
//! 就成了「过期数据」：报告还是那份报告，但测项认得不一样了。
//! 这时候要回答两个问题：
//!
//! 1. **哪些报告的口径漂了？**（漂移清单）
//! 2. **补处理之后台账行会不会变成两份？**（不会 —— 补处理是**刷新**，
//!    不是追加）
//!
//! `ingest` 回答不了这两个问题：它是**新投递受理**通道，
//! 幂等键命中就跳过 —— 恰恰是「改了词典想重算」时最需要的那一步。
//!
//! ## 三态口径
//!
//! | 情形 | 结论 | 台账影响 |
//! |---|---|---|
//! | 幂等键**没命中**（库里没有这份报告） | **非重放对象** | 无（这是新投递，归 `ingest` 管） |
//! | 命中且重算结果与库内**一致** | **一致** | 无 |
//! | 命中但重算结果与库内**不一致** | **漂移** | 无（默认只对账） |
//!
//! `--apply` 时，漂移的报告被**补处理**：台账行**替换**成重算结果，
//! **报告行不新增**（同一份报告）、**不产生第二条台账行**。
//!
//! ## 与幂等的关系
//!
//! 重放的钥匙就是入库用的那把（`sha256(study_no‖内容字节)`）——
//! 同一把钥匙，同一套算法（[`crate::workflow::idempotency::idempotency_key`]）。
//! 所以「这份内容处理过没有」这个问题，重放与入库的答案**永远一致**。

use std::path::Path;

use crate::export::fmt_number;
use crate::ingest::{list_report_dirs, read_report_dir, IngestError};
use crate::parse::extract::ParsedItem;
use crate::state::repo::{IngestRecord, ItemRow, Repo, RepoError};
use crate::workflow::concurrency::{StudyNoGate, DEFAULT_ATTEMPTS, DEFAULT_LEASE_SECS};

/// 重放模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayMode {
    /// **对账**：重新解析并与库里已有条目逐条比对，**不落库**。
    Verify,
    /// **补处理**：漂移的报告**刷新**（替换台账行；报告行不新增）。
    Refresh,
}

/// 漂移的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriftKind {
    /// 条目数变了（补处理后会多出或少掉测值）。
    CountChanged,
    /// 同一条目的内容变了（口径漂移）。
    ItemChanged,
}

impl DriftKind {
    /// 稳定字符串（进日志与报告）。
    pub fn as_str(&self) -> &'static str {
        match self {
            DriftKind::CountChanged => "count_changed",
            DriftKind::ItemChanged => "item_changed",
        }
    }

    /// 中文说明。
    pub fn message(&self) -> &'static str {
        match self {
            DriftKind::CountChanged => "台账行条数变了",
            DriftKind::ItemChanged => "同一条台账行的内容变了",
        }
    }
}

/// 一条漂移明细。
#[derive(Clone, Debug)]
pub struct Drift {
    /// 检查号。
    pub study_no: String,
    /// 漂移种类。
    pub kind: DriftKind,
    /// 人话说明（指出差在哪：第几条、旧值、新值）。
    pub detail: String,
}

/// 重放结果。
#[derive(Clone, Debug, Default)]
pub struct ReplaySummary {
    /// 目录里的报告份数（扫到的）。
    pub scanned: usize,
    /// 幂等键命中的份数（真正重放的）。
    pub replayed: usize,
    /// 幂等键没命中的份数（**非重放对象**：库里没有这份报告）。
    pub not_replayable: usize,
    /// 重算结果与库内一致的份数。
    pub consistent: usize,
    /// 重算结果与库内不一致（漂移）的份数。
    pub drifted: usize,
    /// 实际补处理（刷新）的份数。
    pub refreshed: usize,
    /// 补处理替换掉的旧台账行数。
    pub replaced_items: usize,
    /// 补处理后落下的新台账行数。
    pub new_items: usize,
    /// 漂移明细。
    pub drifts: Vec<Drift>,
    /// 重放后的台账行总数（**补处理前后对照看**）。
    pub ledger_rows: i64,
}

impl ReplaySummary {
    /// 本次重放**有没有改动台账**（补处理会改；对账不会）。
    pub fn touched_ledger(&self) -> bool {
        self.refreshed > 0
    }

    /// 人话摘要（打印到终端）。
    pub fn render(&self) -> String {
        let mut out = format!(
            "重放完成：扫到 {} 份 · 重放 {} 份 · 一致 {} 份 · 漂移 {} 份 · 非重放对象 {} 份",
            self.scanned, self.replayed, self.consistent, self.drifted, self.not_replayable
        );
        if self.touched_ledger() {
            out.push_str(&format!(
                " · 补处理 {} 份（替换旧台账行 {} 条 → 新落 {} 条）",
                self.refreshed, self.replaced_items, self.new_items
            ));
        }
        out.push_str(&format!(" · 台账行总数 {} 行", self.ledger_rows));
        out
    }
}

/// 重放错误。
#[derive(Debug)]
pub enum ReplayError {
    /// 投递目录读不到。
    Ingest(IngestError),
    /// 状态库错误。
    Repo(RepoError),
    /// **并发冲突**：同一检查号正在被别处处理。
    Concurrency(crate::workflow::concurrency::ConcurrencyError),
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::Ingest(e) => write!(f, "{}", e),
            ReplayError::Repo(e) => write!(f, "{}", e),
            ReplayError::Concurrency(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for ReplayError {}

impl From<IngestError> for ReplayError {
    fn from(e: IngestError) -> Self {
        ReplayError::Ingest(e)
    }
}

impl From<RepoError> for ReplayError {
    fn from(e: RepoError) -> Self {
        ReplayError::Repo(e)
    }
}

impl From<crate::workflow::concurrency::ConcurrencyError> for ReplayError {
    fn from(e: crate::workflow::concurrency::ConcurrencyError) -> Self {
        ReplayError::Concurrency(e)
    }
}

/// 重放一个投递目录。
///
/// **与入库同一条读入路径**（[`read_report_dir`]）—— 只有「落不落库」的策略不同。
pub fn replay_inbox(
    repo: &Repo,
    inbox: &Path,
    mode: ReplayMode,
) -> Result<ReplaySummary, ReplayError> {
    // **词典作用域**：这一轮用**同一代**词典 —— 跑到一半换了口径，
    // 漂移清单就会一半旧口径一半新口径，说不清「这份漂移是哪套口径算出来的」。
    let _scope = crate::coerce::dictionary_lifecycle::scope_current();

    let mut summary = ReplaySummary::default();
    let dirs = list_report_dirs(inbox)?;
    for dir in dirs {
        let scanned = match read_report_dir(&dir)? {
            Some(scanned) => scanned,
            None => continue,
        };
        summary.scanned += 1;

        // **幂等预查**：这份内容入库时登记过没有？
        let existing = repo.find_by_idempotency_key(&scanned.idempotency_key)?;
        let existing = match existing {
            Some(row) => row,
            // 库里没有这份报告 → **非重放对象**（新投递归 `ingest` 管）。
            None => {
                summary.not_replayable += 1;
                continue;
            }
        };
        summary.replayed += 1;

        // 重算结果 vs 库内已有条目：逐条比对。
        let stored = repo.items_of_report(existing.id)?;
        let drifts = diff_items(&stored, &scanned.items);
        if drifts.is_empty() {
            summary.consistent += 1;
            continue;
        }
        summary.drifted += 1;
        summary.drifts.extend(drifts.into_iter().map(|d| Drift {
            study_no: scanned.study_no.clone(),
            ..d
        }));

        if mode == ReplayMode::Refresh {
            // **补处理**：台账行**替换**成重算结果，报告行不新增。
            // 同一检查号不并发处理（与入库同一条库级锁口径）。
            let gate = StudyNoGate::new(repo);
            let _lease = gate.acquire(
                &scanned.dir_study_no,
                DEFAULT_LEASE_SECS,
                DEFAULT_ATTEMPTS,
            )?;
            let record = IngestRecord {
                study_no: &scanned.study_no,
                patient_id: scanned.patient_id.as_deref(),
                source_path: Some(&scanned.source_path),
                idempotency_key: &scanned.idempotency_key,
                dictionary_version: scanned.dictionary_version.clone(),
                items: &scanned.items,
                failures: &scanned.failures,
            };
            let refreshed = repo.refresh_ingestion(&record, existing.id)?;
            summary.refreshed += 1;
            summary.replaced_items += refreshed.replaced_items;
            summary.new_items += refreshed.item_count;
        }
    }

    summary.ledger_rows = count_items(repo)?;
    Ok(summary)
}

/// 台账行总数。
fn count_items(repo: &Repo) -> Result<i64, RepoError> {
    repo.connection()
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .map_err(RepoError::from)
}

/// 一条台账行的**签名**（比对口径的最小单位）。
///
/// 只取**业务口径字段**（部位 / 测项 / 归一值 / 单位码 / 强转路径 / 行列）——
/// 主键与时间戳不比（重算出来的行必然是新的）。
fn signature(
    part_key: Option<&str>,
    item_code: Option<&str>,
    value_si: Option<f64>,
    unit_code: Option<&str>,
    path: &str,
    line: usize,
    column: usize,
) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        part_key.unwrap_or("-"),
        item_code.unwrap_or("-"),
        fmt_number(value_si),
        unit_code.unwrap_or("-"),
        path,
        line,
        column
    )
}

/// 新算出来的条目签名。
fn item_signature(item: &ParsedItem) -> String {
    signature(
        item.part_key.as_deref(),
        item.item_code.as_deref(),
        item.value_si,
        item.unit_code.as_deref(),
        item.coercion_path.as_str(),
        item.line,
        item.column,
    )
}

/// 库里已有条目的签名。
fn row_signature(row: &ItemRow) -> String {
    signature(
        row.part_key.as_deref(),
        row.item_code.as_deref(),
        row.value_si,
        row.unit_code.as_deref(),
        &row.coercion_path,
        row.line,
        row.column,
    )
}

/// 逐条比对：**库里已有** vs **重算出来**。
///
/// 返回空 = 一致；非空 = 漂移明细（指出差在第几条）。
///
/// **按顺序比**（库里按主键序 = 落库序 = 解析序）：同一份报告、
/// 同一套词典，重算结果与库内结果**逐条同序**——顺序变了就是口径变了，
/// 这本身就是要报出来的漂移。
fn diff_items(stored: &[ItemRow], fresh: &[ParsedItem]) -> Vec<Drift> {
    let mut drifts = Vec::new();
    if stored.len() != fresh.len() {
        drifts.push(Drift {
            study_no: String::new(),
            kind: DriftKind::CountChanged,
            detail: format!(
                "台账行条数：库内 {} 条 → 重算 {} 条",
                stored.len(),
                fresh.len()
            ),
        });
    }
    for (idx, row) in stored.iter().enumerate() {
        let fresh_sig = match fresh.get(idx) {
            Some(item) => item_signature(item),
            None => break,
        };
        let stored_sig = row_signature(row);
        if stored_sig != fresh_sig {
            drifts.push(Drift {
                study_no: String::new(),
                kind: DriftKind::ItemChanged,
                detail: format!(
                    "第 {} 条：库内 {} → 重算 {}",
                    idx + 1,
                    stored_sig,
                    fresh_sig
                ),
            });
        }
    }
    drifts
}

/// 补处理**待回写**的报告（`pending_writeback` → 重新回写 EMR）。
///
/// 与「口径漂移补处理」（`--apply`）是**两件不同的事**，别混成一条路径：
///
/// | | 台账行 | 要做的事 |
/// |---|---|---|
/// | `--apply`（口径漂移） | **过期了**（旧口径算出来的） | 重算并**刷新**台账行 |
/// | `--retry-pending`（待回写） | **是对的** | 重新发一次回写（EMR 那一步当时没写成） |
///
/// 走的是**同一个回写通道**（[`crate::outbound::retry::WritebackChannel`]）——
/// 退避重试、失败升级、告警、成功收口的口径只有一份。
pub fn retry_pending_writebacks(
    repo: &Repo,
    emr: Option<&crate::outbound::emr::EmrClient>,
    wecom: Option<&crate::outbound::wecom::WeComClient>,
    policy: crate::outbound::retry::RetryPolicy,
) -> Result<PendingRetrySummary, ReplayError> {
    let mut summary = PendingRetrySummary::default();
    let candidates = pending_writeback_study_nos(repo)?;
    summary.candidates = candidates.len();
    if candidates.is_empty() {
        return Ok(summary);
    }

    let channel = crate::outbound::retry::WritebackChannel::new(repo, emr, wecom, policy);
    for study_no in candidates {
        match channel.retry_pending(&study_no) {
            Ok(result) if result.written => {
                summary.written += 1;
                summary.details.push(format!(
                    "{}：回写成功（尝试 {} 次，EMR 状态 {}）",
                    study_no,
                    result.attempts,
                    result
                        .receipt
                        .as_ref()
                        .map(|r| r.status)
                        .unwrap_or_default()
                ));
            }
            Ok(result) => {
                // 写失败：状态**如实留在** `pending_writeback`，台账行保留，告警已发。
                summary.failed += 1;
                summary.details.push(format!(
                    "{}：补处理仍失败（尝试 {} 次，错误码 {}）",
                    study_no,
                    result.attempts,
                    result.error_code().unwrap_or("未知")
                ));
            }
            Err(crate::outbound::retry::RetryError::NotPending { status, .. }) => {
                summary.skipped += 1;
                summary.details.push(format!(
                    "{}：不在待回写状态（当前 {}），跳过",
                    study_no, status
                ));
            }
            Err(crate::outbound::retry::RetryError::AlreadyClaimed(_)) => {
                summary.skipped += 1;
                summary.details.push(format!("{}：回写幂等键已登记，不重复写", study_no));
            }
            Err(err) => {
                summary.failed += 1;
                summary.details.push(format!("{}：补处理失败（{}）", study_no, err));
            }
        }
    }
    Ok(summary)
}

/// 待回写的检查号（库里状态为 `pending_writeback` 的报告）。
fn pending_writeback_study_nos(repo: &Repo) -> Result<Vec<String>, RepoError> {
    let mut stmt = repo.connection().prepare(
        "SELECT study_no FROM reports WHERE status = ?1 ORDER BY study_no",
    )?;
    let rows = stmt.query_map(
        [crate::workflow::state::Status::PendingWriteback.as_str()],
        |row| row.get::<_, String>(0),
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 补处理待回写报告的结果。
#[derive(Clone, Debug, Default)]
pub struct PendingRetrySummary {
    /// 待回写的报告份数（候选）。
    pub candidates: usize,
    /// 补处理成功（回到 `written_back`）的份数。
    pub written: usize,
    /// 补处理仍失败的份数（**仍留在** `pending_writeback`）。
    pub failed: usize,
    /// 跳过的份数（状态不对 / 幂等键已登记）。
    pub skipped: usize,
    /// 逐份说明。
    pub details: Vec<String>,
}

impl PendingRetrySummary {
    /// 人话摘要。
    pub fn render(&self) -> String {
        format!(
            "待回写补处理完成：候选 {} 份 · 补回成功 {} 份 · 仍失败 {} 份 · 跳过 {} 份",
            self.candidates, self.written, self.failed, self.skipped
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::repo::Repo;
    use std::fs;
    use std::path::PathBuf;

    /// 造一个投递目录（一份正常报告 + 一份缺单位报告）。
    fn make_inbox(base: &Path) -> PathBuf {
        let inbox = base.join("inbox");
        fs::create_dir_all(inbox.join("S-A")).unwrap();
        fs::create_dir_all(inbox.join("S-B")).unwrap();
        fs::write(
            inbox.join("S-A/report.txt"),
            "病历号：PID-EXAMPLE\n超声所见\n左肾积水 3.2cm\n",
        )
        .unwrap();
        fs::write(inbox.join("S-B/report.txt"), "病历号：PID-EXAMPLE\n左肾积水 3.2\n").unwrap();
        inbox
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("urs-replay-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn 库里没有的算非重放对象() {
        let dir = tmp_dir("not-replayable");
        let inbox = make_inbox(&dir);
        let repo = Repo::open_in_memory().unwrap();
        let summary = replay_inbox(&repo, &inbox, ReplayMode::Verify).unwrap();
        assert_eq!(summary.scanned, 2);
        assert_eq!(summary.replayed, 0);
        assert_eq!(summary.not_replayable, 2);
        assert_eq!(summary.ledger_rows, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 重算结果一致时判一致且不动台账() {
        let dir = tmp_dir("consistent");
        let inbox = make_inbox(&dir);
        let repo = Repo::open_in_memory().unwrap();
        crate::ingest::ingest_inbox(&repo, &inbox).unwrap();
        let before = count_items(&repo).unwrap();

        let summary = replay_inbox(&repo, &inbox, ReplayMode::Verify).unwrap();
        assert_eq!(summary.replayed, 2);
        assert_eq!(summary.consistent, 2);
        assert_eq!(summary.drifted, 0);
        assert_eq!(summary.refreshed, 0);
        assert_eq!(count_items(&repo).unwrap(), before, "对账不得改台账");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 补处理刷新台账行且不产生第二条报告行() {
        let dir = tmp_dir("refresh");
        let inbox = make_inbox(&dir);
        let repo = Repo::open_in_memory().unwrap();
        crate::ingest::ingest_inbox(&repo, &inbox).unwrap();
        let reports_before: i64 = repo
            .connection()
            .query_row("SELECT COUNT(*) FROM reports", [], |row| row.get(0))
            .unwrap();
        let items_before = count_items(&repo).unwrap();

        // 把库里那份报告的台账行**改坏**（模拟旧口径产物），再补处理。
        repo.connection()
            .execute("UPDATE items SET value_si = 99.0", [])
            .unwrap();

        let summary = replay_inbox(&repo, &inbox, ReplayMode::Refresh).unwrap();
        assert_eq!(summary.drifted, 1, "改坏的那份应判漂移");
        assert_eq!(summary.refreshed, 1);
        assert_eq!(summary.replaced_items, 1);
        assert_eq!(summary.new_items, 1);

        // 值被刷回 32（不是 99）。
        let value: Option<f64> = repo
            .connection()
            .query_row("SELECT value_si FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, Some(32.0));

        // **报告行不新增**（同一份报告），台账行数也不翻倍。
        let reports_after: i64 = repo
            .connection()
            .query_row("SELECT COUNT(*) FROM reports", [], |row| row.get(0))
            .unwrap();
        assert_eq!(reports_after, reports_before);
        assert_eq!(count_items(&repo).unwrap(), items_before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 漂移明细指出差在哪一条() {
        let dir = tmp_dir("drift-detail");
        let inbox = make_inbox(&dir);
        let repo = Repo::open_in_memory().unwrap();
        crate::ingest::ingest_inbox(&repo, &inbox).unwrap();
        repo.connection()
            .execute("UPDATE items SET value_si = 99.0", [])
            .unwrap();
        let summary = replay_inbox(&repo, &inbox, ReplayMode::Verify).unwrap();
        assert_eq!(summary.drifts.len(), 1);
        let drift = &summary.drifts[0];
        assert_eq!(drift.study_no, "S-A");
        assert_eq!(drift.kind, DriftKind::ItemChanged);
        assert!(drift.detail.contains("第 1 条"));
        assert!(drift.detail.contains("99"));
        assert!(drift.detail.contains("32"));
        let _ = fs::remove_dir_all(&dir);
    }
}
