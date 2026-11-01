//! 幂等键与事务边界：重复投递不产生第二条台账行。
//!
//! ## 这层解决什么
//!
//! 超声工作站会**重发**报告（网络抖动、有人手工再点一次导出），
//! 守护进程也会**重投**（进程重启后从库里续跑）。
//! 没有幂等的话，重发一次就在台账里多一行 —— 而台账是**要跟 RIS 对账、要给医师看**的东西，
//! 多出来的一行会被当成「这台机器多测了一次」，比漏测更难解释。
//!
//! ## 一把钥匙，两个用途
//!
//! 幂等键 = `sha256(study_no ‖ 报告内容)`，**同一把钥匙**用于两处登记：
//!
//! | 用途（scope） | 登记什么 | 谁在用 |
//! |---|---|---|
//! | [`SCOPE_INGEST`] | 这份报告已入库（台账行已落） | 入站监听 / 批量入库 |
//! | [`SCOPE_WRITEBACK`] | 这份报告已回写 EMR | 回写通道（Task 41） |
//!
//! **同一个键在两个用途下各登记一次是合法的**（一份报告要入库、也要回写），
//! 所以幂等表的唯一索引是 **(键, 用途)** 组合而不是单列 ——
//! 见 [`crate::state::schema`] 的迁移版本 2。
//!
//! ## 源产品同构证据（机制改写，非代码移植）
//!
//! `github_ref/pgrust/crates/_support/mcx/src/aset.rs` + `tests.rs`：
//!
//! - `aset.rs` 的 `is_dedicated(layout)` 是**纯函数** —— 分配与释放走**同一个决策**，
//!   所以「这次分配走池子、那次释放走全局」这种错配根本不可能发生；
//! - `tests.rs` 的 `limit_enforced_via_try_reserve` 断言
//!   **「失败的申请不扣账」**（`failed reservation charged nothing`）——
//!   申请失败后额度计数**原封不动**。
//!
//! 本项目改写成同一立场：**幂等查重与写库走同一把钥匙**（决策与执行不分家），
//! **事务失败不落半条台账**（失败的写入不扣账）。
//! 源侧是内存上下文，本侧是 SQLite 事务 —— 立场同构，代码不同源。

use crate::state::idempotency_key_bytes;
use crate::state::repo::{AuditEntry, IdempotencyRow, IngestRecord, Repo, RepoError};

/// 登记用途：**这份报告已入库**（台账行已落）。
pub const SCOPE_INGEST: &str = "ingest";

/// 登记用途：**这份报告已回写 EMR**。
///
/// 预留给出站回写通道（Task 41）使用 —— 现在没有人登记它，
/// 但**索引与语义先立好**：等回写通道落地时不必再改 schema。
pub const SCOPE_WRITEBACK: &str = "writeback";

/// 幂等键 = `sha256(study_no ‖ 报告内容字节)`。
///
/// 与 [`crate::state::idempotency_key_bytes`] **同一个函数** ——
/// 幂等键只有一个算法、一个拼接顺序，散开写迟早会写出两个版本。
pub fn idempotency_key(study_no: &str, body: &[u8]) -> String {
    idempotency_key_bytes(study_no, body)
}

/// 幂等键 = `sha256(study_no ‖ 报告内容)`（字符串版）。
pub fn idempotency_key_text(study_no: &str, body: &str) -> String {
    idempotency_key_bytes(study_no, body.as_bytes())
}

/// **一次幂等写入的结果**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdempotencyOutcome {
    /// **首次受理**：这笔写请求被接受，台账行已落。
    Committed {
        /// 报告主键。
        report_id: i64,
        /// 落库的条目数。
        item_count: usize,
        /// 落库的失败条目数。
        failure_count: usize,
    },
    /// **幂等命中**：这份内容已经处理过，跳过写入。
    Duplicate {
        /// 已有报告的主键（**不新建**）。
        report_id: i64,
        /// 已有报告本来就有几条台账行（跳过时原样保留）。
        item_count: usize,
    },
}

impl IdempotencyOutcome {
    /// 这笔写请求是否**被受理**（`true` = 落了台账；`false` = 被幂等跳过）。
    pub fn is_committed(&self) -> bool {
        matches!(self, IdempotencyOutcome::Committed { .. })
    }

    /// 是否**被幂等跳过**。
    pub fn is_duplicate(&self) -> bool {
        matches!(self, IdempotencyOutcome::Duplicate { .. })
    }

    /// 台账行数（受理与跳过都返回 —— 跳过时是「本来就有几行」）。
    pub fn item_count(&self) -> usize {
        match self {
            IdempotencyOutcome::Committed { item_count, .. } => *item_count,
            IdempotencyOutcome::Duplicate { item_count, .. } => *item_count,
        }
    }

    /// 落库的失败条目数（**只有受理时才有值** —— 跳过时不新增失败条目）。
    pub fn failures(&self) -> usize {
        match self {
            IdempotencyOutcome::Committed { failure_count, .. } => *failure_count,
            IdempotencyOutcome::Duplicate { .. } => 0,
        }
    }

    /// 报告主键（受理与跳过都是同一个 —— 跳过时是**已有**那份的主键）。
    pub fn report_id(&self) -> i64 {
        match self {
            IdempotencyOutcome::Committed { report_id, .. } => *report_id,
            IdempotencyOutcome::Duplicate { report_id, .. } => *report_id,
        }
    }
}

/// **幂等闸**：所有会写台账的入口都从这里过。
///
/// 调用方不再自己判断「这份处理过没有」——
/// 判重、跳过、留痕三件事在这里做完，调用方拿到的就是结论。
pub struct IdempotencyGuard<'a> {
    repo: &'a Repo,
}

impl<'a> IdempotencyGuard<'a> {
    /// 建一个幂等闸（挂在状态库上）。
    pub fn new(repo: &'a Repo) -> Self {
        IdempotencyGuard { repo }
    }

    /// **取件前的幂等预查**：这份内容登记过没有？
    ///
    /// 供入站监听在**动手之前**先问一句（避免白解析一遍）。
    /// 注意这只是**快路径**：真正的兜底是落库时的唯一索引
    /// （预查与落库之间有窗口，并发投递会从这个窗口钻进来）。
    pub fn pre_check(&self, key: &str) -> Result<Option<IdempotencyRow>, RepoError> {
        self.repo.find_idempotency_scoped(key, SCOPE_INGEST)
    }

    /// **幂等写入**：落台账；命中幂等键就跳过并留痕。
    ///
    /// - 首次 → 事务落库（报告行 + 条目 + 审计留痕），返回 [`IdempotencyOutcome::Committed`]。
    /// - 命中 → **不产生第二条台账行**，落一条 `duplicate_skipped` 留痕，
    ///   返回 [`IdempotencyOutcome::Duplicate`]。
    /// - 台账行不合法 → 整笔回滚，返回 [`RepoError::InvalidItem`]，**不落半条台账**。
    ///
    /// **重复投递为什么也要留痕**：不留痕的话，运维看到「昨天收到 30 份、
    /// 库里只有 28 份」根本不知道少的 2 份是丢了还是被跳过了。
    pub fn commit_ingestion(
        &self,
        rec: &IngestRecord,
    ) -> Result<IdempotencyOutcome, RepoError> {
        match self.repo.commit_ingestion(rec) {
            Ok(result) => Ok(IdempotencyOutcome::Committed {
                report_id: result.report_id,
                item_count: result.item_count,
                failure_count: result.failure_count,
            }),
            Err(RepoError::AlreadyExists(_)) => {
                // 命中唯一约束：**跳过写入**，但要把「为什么没有第二条台账行」记下来。
                //
                // **两种「已存在」要分清**（运维处置方式不同）：
                //
                // | 情形 | 拦下它的约束 | 含义 |
                // |---|---|---|
                // | 同一份**内容** | 幂等键唯一索引 | 工作站重发 —— 常态，跳过即可 |
                // | 同一**检查号**、内容不同 | `reports.study_no` 唯一索引 | 工作站改过又重导 / 两个来源推同一份检查 —— **要追** |
                //
                // 两种都**不产生第二条台账行**：台账是要跟 RIS 对账、要统计
                // 「本月积水病例数」的东西，同一个检查号出两行会被当成「多测了一次」。
                let (report_id, item_count, detail) = match self
                    .repo
                    .find_by_idempotency_key(rec.idempotency_key)?
                {
                    Some(row) => {
                        let count = self.repo.count_items_of_report(row.id)?;
                        (
                            Some(row.id),
                            count,
                            "重复投递：幂等键命中（scope=ingest），跳过且不产生第二条台账行",
                        )
                    }
                    // 幂等键没命中 → 撞的是**检查号**：同一个检查号来了不同内容。
                    None => match self.repo.find_by_study_no(rec.study_no)? {
                        Some(row) => {
                            let count = self.repo.count_items_of_report(row.id)?;
                            (
                                Some(row.id),
                                count,
                                "重复投递：检查号已有报告（内容与已入库报告不同），\
                                 跳过且不产生第二条台账行",
                            )
                        }
                        // 报告行不在（登记行在的极端情况）——不编造主键。
                        None => (
                            None,
                            0,
                            "重复投递：命中唯一约束但找不到已有报告，跳过且不产生第二条台账行",
                        ),
                    },
                };
                record_duplicate_skipped_detail(
                    self.repo,
                    report_id,
                    rec.study_no,
                    rec.idempotency_key,
                    &rec.dictionary_version,
                    detail,
                )?;
                Ok(IdempotencyOutcome::Duplicate {
                    report_id: report_id.unwrap_or_default(),
                    item_count: item_count as usize,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// **已受理的写请求数**（某用途下）。
    ///
    /// 口径：每一笔真正落库的写入都在幂等表登记一行；
    /// 被幂等拦下的**不登记**（登记在事务里，随回滚一起撤销）。
    pub fn accepted_write_requests(&self, scope: &str) -> Result<i64, RepoError> {
        self.repo.count_accepted_writes(scope)
    }

    /// **被幂等拦下的写请求数**。
    pub fn skipped_write_requests(&self) -> Result<i64, RepoError> {
        self.repo.count_skipped_writes()
    }

    /// 落一条 `duplicate_skipped` 留痕。
    ///
    /// **唯一写入出口** —— 入站监听的预查与落库闸的命中走同一条路径，
    /// 免得两处各写一份形状不一样的留痕（运维按 `detail` 聚合就漏一类）。
    pub fn record_duplicate_skipped(
        &self,
        report_id: Option<i64>,
        study_no: &str,
        idempotency_key: &str,
        dictionary_version: &str,
        scope: &str,
    ) -> Result<i64, RepoError> {
        record_duplicate_skipped(
            self.repo,
            report_id,
            study_no,
            idempotency_key,
            dictionary_version,
            scope,
        )
    }
}

/// 落一条 `duplicate_skipped` 留痕（**自由函数版**：不持有 `Repo` 也能用）。
pub fn record_duplicate_skipped(
    repo: &Repo,
    report_id: Option<i64>,
    study_no: &str,
    idempotency_key: &str,
    dictionary_version: &str,
    scope: &str,
) -> Result<i64, RepoError> {
    record_duplicate_skipped_detail(
        repo,
        report_id,
        study_no,
        idempotency_key,
        dictionary_version,
        &format!(
            "重复投递：幂等键命中（scope={}），跳过且不产生第二条台账行",
            scope
        ),
    )
}

/// 落一条 `duplicate_skipped` 留痕，**留痕文案自己给**。
///
/// 两种「已存在」要说得清（见 [`IdempotencyGuard::commit_ingestion`]）：
/// 幂等键命中（同一份内容重发）与检查号唯一约束命中（同一检查号、内容不同）。
/// 运维处置方式不同 —— 前者跳过即可，后者**要追**。
pub fn record_duplicate_skipped_detail(
    repo: &Repo,
    report_id: Option<i64>,
    study_no: &str,
    idempotency_key: &str,
    dictionary_version: &str,
    detail: &str,
) -> Result<i64, RepoError> {
    repo.append_audit(&AuditEntry {
        report_id,
        study_no: Some(study_no.to_string()),
        action: crate::workflow::audit::ACTION_DUPLICATE_SKIPPED.to_string(),
        actor: Some("system".to_string()),
        error_code: None,
        line: None,
        column: None,
        raw_snippet: None,
        // 摘要哈希与幂等键同源 —— 留痕能直接指回是哪份内容被判重。
        input_digest: Some(idempotency_key.to_string()),
        dictionary_version: Some(dictionary_version.to_string()),
        detail: Some(detail.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_report;
    use crate::parse::ParsedItem;

    const BODY: &str = "左肾积水 3.2cm";

    fn record<'a>(
        study_no: &'a str,
        key: &'a str,
        items: &'a [ParsedItem],
    ) -> IngestRecord<'a> {
        IngestRecord {
            study_no,
            patient_id: Some("PID-EXAMPLE"),
            source_path: Some("inbox/SAMPLE-A/report.txt"),
            idempotency_key: key,
            dictionary_version: crate::dictionary_version(),
            items,
            failures: &[],
        }
    }

    fn count(repo: &Repo, table: &str) -> i64 {
        repo.connection()
            .query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn 幂等键与状态库口径同源() {
        let key = idempotency_key_text("SAMPLE-A", BODY);
        assert_eq!(key, crate::state::idempotency_key("SAMPLE-A", BODY));
        assert_eq!(
            key,
            idempotency_key("SAMPLE-A", BODY.as_bytes()),
            "字节版与字符串版必须是同一把钥匙"
        );
        assert_eq!(key.len(), 64);
    }

    #[test]
    fn 首次受理落台账行() {
        let repo = Repo::open_in_memory().unwrap();
        let guard = IdempotencyGuard::new(&repo);
        let outcome = parse_report(BODY);
        let key = idempotency_key_text("SAMPLE-A", BODY);
        let result = guard
            .commit_ingestion(&record("SAMPLE-A", &key, &outcome.items))
            .unwrap();
        assert!(result.is_committed());
        assert!(!result.is_duplicate());
        assert_eq!(result.item_count(), 1);
        assert_eq!(count(&repo, "items"), 1);
        assert_eq!(guard.accepted_write_requests(SCOPE_INGEST).unwrap(), 1);
    }

    #[test]
    fn 命中幂等键即跳过并记审计() {
        let repo = Repo::open_in_memory().unwrap();
        let guard = IdempotencyGuard::new(&repo);
        let outcome = parse_report(BODY);
        let key = idempotency_key_text("SAMPLE-A", BODY);
        let rec = record("SAMPLE-A", &key, &outcome.items);
        guard.commit_ingestion(&rec).unwrap();

        let second = guard.commit_ingestion(&rec).unwrap();
        assert!(second.is_duplicate(), "重复投递应被判为幂等命中");
        assert_eq!(second.item_count(), 1, "跳过后原台账行原样保留");
        assert_eq!(
            count(&repo, "items"),
            1,
            "重复投递不得产生第二条台账行"
        );
        assert_eq!(guard.accepted_write_requests(SCOPE_INGEST).unwrap(), 1);
        assert_eq!(guard.skipped_write_requests().unwrap(), 1);

        // 留痕：跳过这件事要说得清（否则运维看不出少的行去哪了）。
        let (action, digest, detail): (String, String, String) = repo
            .connection()
            .query_row(
                "SELECT action, input_digest, detail FROM audit_log \
                 WHERE action = 'duplicate_skipped'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(action, "duplicate_skipped");
        assert_eq!(digest, key, "留痕摘要与幂等键同源");
        assert!(detail.contains("不产生第二条台账行"));
    }

    #[test]
    fn 重放三次台账行数与写请求数均不变() {
        let repo = Repo::open_in_memory().unwrap();
        let guard = IdempotencyGuard::new(&repo);
        let outcome = parse_report(BODY);
        let key = idempotency_key_text("SAMPLE-A", BODY);
        let rec = record("SAMPLE-A", &key, &outcome.items);

        let mut committed = 0;
        for _ in 0..3 {
            if guard.commit_ingestion(&rec).unwrap().is_committed() {
                committed += 1;
            }
        }
        assert_eq!(committed, 1, "三次重放只受理一次");
        assert_eq!(count(&repo, "items"), 1, "台账行数不变");
        assert_eq!(count(&repo, "reports"), 1, "报告行数不变");
        assert_eq!(
            guard.accepted_write_requests(SCOPE_INGEST).unwrap(),
            1,
            "写请求数不变"
        );
        assert_eq!(guard.skipped_write_requests().unwrap(), 2, "两次被跳过");
        // 审计链也不该重复「接收」——重复投递不是又收了一份。
        let received: i64 = repo
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE action = 'received'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(received, 1, "接收留痕只应有一条");
    }

    #[test]
    fn 非法条目触发整体回滚台账行数不增() {
        let repo = Repo::open_in_memory().unwrap();
        let guard = IdempotencyGuard::new(&repo);
        let outcome = parse_report(BODY);
        let mut items = outcome.items.clone();
        // 注入一条**不合法**的台账行：有归一值、没单位码。
        let mut bad = items[0].clone();
        bad.unit_code = None;
        items.push(bad);

        let key = idempotency_key_text("SAMPLE-BAD", BODY);
        let err = guard
            .commit_ingestion(&record("SAMPLE-BAD", &key, &items))
            .unwrap_err();
        assert!(
            matches!(err, RepoError::InvalidItem(_)),
            "非法条目应判为台账行不合法，实际：{}",
            err
        );

        // **不落半条台账**：报告行、条目、审计、幂等登记全部撤销。
        assert_eq!(count(&repo, "items"), 0, "不得落条目");
        assert_eq!(count(&repo, "reports"), 0, "不得落报告行");
        assert_eq!(count(&repo, "audit_log"), 0, "不得留审计");
        assert_eq!(count(&repo, "idempotency"), 0, "不得登记幂等键");
        assert_eq!(
            guard.accepted_write_requests(SCOPE_INGEST).unwrap(),
            0,
            "失败的写入不扣账"
        );
    }

    #[test]
    fn 同一把钥匙在两个用途下可各登记一次() {
        let repo = Repo::open_in_memory().unwrap();
        let guard = IdempotencyGuard::new(&repo);
        let outcome = parse_report(BODY);
        let key = idempotency_key_text("SAMPLE-C", BODY);
        let rec = record("SAMPLE-C", &key, &outcome.items);
        guard.commit_ingestion(&rec).unwrap();

        // 回写用途另登记一次：同一把钥匙，两个用途 —— 不该撞唯一约束。
        repo.append_audit(&AuditEntry {
            report_id: None,
            study_no: Some("SAMPLE-C".to_string()),
            action: crate::workflow::audit::ACTION_WRITTEN_BACK.to_string(),
            actor: Some("system".to_string()),
            error_code: None,
            line: None,
            column: None,
            raw_snippet: None,
            input_digest: Some(key.clone()),
            dictionary_version: Some(crate::dictionary_version()),
            detail: Some("回写用途登记".to_string()),
        })
        .unwrap();

        let scoped = repo
            .find_idempotency_scoped(&key, SCOPE_INGEST)
            .unwrap();
        assert!(scoped.is_some(), "入库用途登记仍在");
        assert_eq!(scoped.unwrap().scope, SCOPE_INGEST);
    }

    #[test]
    fn 取件前预查命中即知已处理过() {
        let repo = Repo::open_in_memory().unwrap();
        let guard = IdempotencyGuard::new(&repo);
        let outcome = parse_report(BODY);
        let key = idempotency_key_text("SAMPLE-D", BODY);
        assert!(guard.pre_check(&key).unwrap().is_none());
        guard
            .commit_ingestion(&record("SAMPLE-D", &key, &outcome.items))
            .unwrap();
        let hit = guard.pre_check(&key).unwrap();
        assert!(hit.is_some(), "入库后预查应命中");
        assert_eq!(hit.unwrap().scope, SCOPE_INGEST);
    }
}
