//! 报告处理器：候选 → 解析 → 强转 → 落库。
//!
//! **为什么抽成 trait**：并发上限与优雅退出是**调度器**的性质，与「怎么处理」无关。
//! 生产用 [`IngestProcessor`]（与 `ingest` 子命令**同一条**解析与落库路径，
//! 不另开第二套口径）；测试可以换一个「慢处理器」来观测并发峰值 ——
//! 调度器不变，测出来的峰值就是调度器自己的性质。
//!
//! **幂等口径**：本处理器用 `sha256(study_no‖正文)`（[`crate::state::idempotency_key`]），
//! 与批量入库路径**同一个函数**；入站监听预查用的是同一算法的字节版
//! （UTF-8 报告两者逐字节相同）。

use crate::ingest::hl7;
use crate::ingest::patient_id_of;
use crate::ingest::watcher::{Candidate, CandidateKind};
use crate::parse::errors::Failure;
use crate::parse::parse_report;
use crate::state::idempotency_key_bytes;
use crate::state::repo::{IngestRecord, Repo, RepoError};
use crate::workflow::concurrency::{
    ConcurrencyError, StudyNoGate, DEFAULT_ATTEMPTS, DEFAULT_LEASE_SECS,
};

/// 一次处理的结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessOutcome {
    /// 落库的结构化条目数。
    pub items: usize,
    /// 落库的失败条目数（进失败清单）。
    pub failures: usize,
    /// 是否因幂等键命中被跳过（重复投递，**不产生第二条台账行**）。
    pub duplicate: bool,
    /// 是否**编码判不出**（混编 / 非文本）：显式失败，**不猜转码**。
    pub undecodable: bool,
    /// 落库的报告主键（**被幂等跳过时为空**）。
    ///
    /// 对拍那一步要拿着报告行去查台账 —— 检查号在 HL7 通道上取自 `OBR-3`，
    /// 与投递目录名可能不同，所以这里给**主键**而不是检查号。
    pub report_id: Option<i64>,
}

impl ProcessOutcome {
    /// 这份报告是否算「处理成功」（重复投递与读不出正文都不算失败条目）。
    pub fn is_ok(&self) -> bool {
        !self.undecodable
    }
}

/// 处理错误。
#[derive(Debug)]
pub enum ProcessError {
    /// 状态库错误。
    Repo(RepoError),
    /// HL7 v2 报告读取 / 解码错误。
    Hl7(hl7::Hl7Error),
    /// **并发冲突**：同一检查号正在被别处处理，本次不并发处理。
    ///
    /// 这不是数据坏了 —— 交还事件源，下一轮再取（源文件没动，重试是安全的）。
    Concurrency(ConcurrencyError),
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessError::Repo(e) => write!(f, "{}", e),
            ProcessError::Hl7(e) => write!(f, "{}", e),
            ProcessError::Concurrency(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for ProcessError {}

impl From<RepoError> for ProcessError {
    fn from(e: RepoError) -> Self {
        ProcessError::Repo(e)
    }
}

impl From<hl7::Hl7Error> for ProcessError {
    fn from(e: hl7::Hl7Error) -> Self {
        ProcessError::Hl7(e)
    }
}

impl From<ConcurrencyError> for ProcessError {
    fn from(e: ConcurrencyError) -> Self {
        ProcessError::Concurrency(e)
    }
}

/// 报告处理器。
///
/// `process` 由**工作线程**调用；`repo` 是该线程**自己持有的一条** SQLite 连接
/// （WAL 模式下多连接可并行写，写入由 SQLite 自己串行化）。
pub trait ReportProcessor: Send + Sync {
    /// 处理一份候选报告。
    fn process(
        &self,
        repo: &Repo,
        candidate: &Candidate,
    ) -> Result<ProcessOutcome, ProcessError>;
}

/// 生产处理器：解析 → 强转 → 落库。
#[derive(Clone, Copy, Debug, Default)]
pub struct IngestProcessor;

impl ReportProcessor for IngestProcessor {
    fn process(
        &self,
        repo: &Repo,
        candidate: &Candidate,
    ) -> Result<ProcessOutcome, ProcessError> {
        // **库级串行**：同一检查号不并发处理。
        //
        // 调度器里那份在途集合只挡得住「本进程内的两个 worker」；
        // 「定时批处理与守护进程重叠跑」是**两个进程**撞同一份报告 ——
        // 库级租约锁才挡得住（重启后也成立）。
        //
        // 锁的键取**投递目录名**（这次投递的身份）：工作站重发、人工重投
        // 落回同一个目录，重投路径自然被串住。
        // 报告级的唯一性（`reports.study_no`）另有一道 —— 那是「检查号」口径，
        // 与「投递身份」不是一回事（HL7 通道的检查号取自 OBR-3）。
        let gate = StudyNoGate::new(repo);
        let _lease = gate.acquire(
            &candidate.study_no,
            DEFAULT_LEASE_SECS,
            DEFAULT_ATTEMPTS,
        )?;

        // **词典作用域**：这份报告从解析到落库用**同一代**词典。
        // 运维在处理途中热更新词典，这一行的口径留痕仍说得清是哪一套。
        let _scope = crate::coerce::dictionary_lifecycle::scope_current();
        // 编码判别：**不猜转码**。判不出来 → 显式失败，
        // 台账里出现一个「看起来像数字」的乱码值，比缺这一行更危险。
        let decoded = match candidate.decoded() {
            Ok(decoded) => decoded,
            Err(e) => {
                // 这一份**不产出条目**，但要落一条带错误码的失败条目：
                // 运维在失败清单里看得见「哪一份没解析出来、为什么」。
                let failure = Failure::new(e.error_code(), 0, 0, 0, e.reason())
                    .with_detail_code(e.detail_code());
                let key = idempotency_key_bytes(&candidate.study_no, &candidate.raw_bytes);
                let record = IngestRecord {
                    study_no: &candidate.study_no,
                    patient_id: None,
                    source_path: Some(&candidate.path.to_string_lossy()),
                    idempotency_key: &key,
                    dictionary_version: crate::dictionary_version(),
                    items: &[],
                    failures: std::slice::from_ref(&failure),
                };
                // 走**幂等闸**：命中即跳过并留痕（不产生第二条失败条目）。
                let guard = crate::workflow::idempotency::IdempotencyGuard::new(repo);
                return match guard.commit_ingestion(&record) {
                    Ok(outcome) => Ok(ProcessOutcome {
                        items: outcome.item_count(),
                        failures: if outcome.is_committed() { 1 } else { 0 },
                        duplicate: outcome.is_duplicate(),
                        undecodable: true,
                        report_id: Some(outcome.report_id()),
                    }),
                    Err(e) => Err(ProcessError::Repo(e)),
                };
            }
        };
        let body = decoded.text.as_str();

        // 两条入站通道汇到同一个解析内核：自由文本直接喂，HL7 先由适配层取
        // 权威字段与所见文本 —— **不另开一条抽取路径**。
        let (study_no, patient_id, outcome) = match candidate.kind {
            CandidateKind::Hl7 => {
                let report = hl7::adapt(body);
                // 检查号以 **OBR-3** 为准（与 RIS 同源）；OBR 缺失时退回投递目录名，
                // 缺段本身已作为失败条目落进失败清单。
                let study_no = report.study_no.clone().unwrap_or_else(|| candidate.study_no.clone());
                (study_no, report.patient_id.clone(), report.outcome)
            }
            CandidateKind::Txt => {
                let outcome = parse_report(body);
                (
                    candidate.study_no.clone(),
                    patient_id_of(body),
                    outcome,
                )
            }
        };

        // 幂等键按**投递文件的字节**算：与监听侧的预查同源（任何编码都一致）。
        let key = idempotency_key_bytes(&study_no, &candidate.raw_bytes);

        let source = candidate.path.to_string_lossy().to_string();
        let record = IngestRecord {
            study_no: &study_no,
            patient_id: patient_id.as_deref(),
            source_path: Some(&source),
            idempotency_key: &key,
            dictionary_version: crate::dictionary_version(),
            items: &outcome.items,
            failures: &outcome.failures,
        };

        // 走**幂等闸**：查重、跳过、留痕三件事在闸里做完，
        // 调用方拿到的就是结论（「已落库」还是「被幂等跳过」）。
        let guard = crate::workflow::idempotency::IdempotencyGuard::new(repo);
        match guard.commit_ingestion(&record) {
            Ok(outcome) => Ok(ProcessOutcome {
                items: outcome.item_count(),
                failures: if outcome.is_committed() {
                    outcome.failures()
                } else {
                    0
                },
                duplicate: outcome.is_duplicate(),
                undecodable: false,
                report_id: Some(outcome.report_id()),
            }),
            Err(e) => Err(ProcessError::Repo(e)),
        }
    }
}

/// **对拍处理器**：在内层处理器之后接一段**对拍入账**。
///
/// ## 为什么是装饰器而不是改 `IngestProcessor`
///
/// 「解析 → 强转 → 落库」与「与 RIS 历史对拍」是**两件事**：
/// 前者**必须**成功（不落台账这份报告就没了），后者**可以失败**
/// （RIS 够不着只是「对不上历史」，测值照样落台账、照样等医师确认）。
///
/// 把对拍塞进 `IngestProcessor` 会让「RIS 慢」拖住「台账落库」。
/// 装饰器把两件事分开：内层照旧落库，外层**尽力而为**地对拍一次。
///
/// ## 对拍失败不影响落库
///
/// RIS 够不着 → 内层对拍层显式落 `comparison_skipped` + 上游错误码
/// （**不产差值**、不冒充首次）—— 报告照常进人工确认队列。
#[derive(Clone, Debug)]
pub struct ComparingProcessor<P> {
    /// 内层处理器（生产是 [`IngestProcessor`]）。
    inner: P,
    /// RIS 出站读客户端（**没配就是 `None`** → 对拍显式跳过，不猜）。
    ris: Option<crate::compare::ris_client::RisClient>,
}

impl<P> ComparingProcessor<P> {
    /// 造一个带对拍段的处理器。
    pub fn new(
        inner: P,
        ris: Option<crate::compare::ris_client::RisClient>,
    ) -> ComparingProcessor<P> {
        ComparingProcessor { inner, ris }
    }
}

impl<P: ReportProcessor> ReportProcessor for ComparingProcessor<P> {
    fn process(
        &self,
        repo: &Repo,
        candidate: &Candidate,
    ) -> Result<ProcessOutcome, ProcessError> {
        // 1) 先落库（**这一段必须成功**）。
        let outcome = self.inner.process(repo, candidate)?;

        // 2) 再对拍（**尽力而为**）。
        //
        // 重复投递：这份已经比过了（幂等键命中 → 不重复对拍）；
        // 编码判不出：台账里没有可对拍的测值（对拍没有对象）。
        if outcome.duplicate || outcome.undecodable || outcome.items == 0 {
            return Ok(outcome);
        }
        let report_id = match outcome.report_id {
            Some(id) => id,
            None => return Ok(outcome),
        };
        let report = match repo.find_by_id(report_id)? {
            Some(row) => row,
            None => return Ok(outcome),
        };
        // 对拍入账：RIS 够不着时这一层自己处理（落 `comparison_skipped`），
        // 不把「上游不可达」当成「这份报告处理失败」。
        match crate::compare::pipeline::compare_report_row(repo, &report, self.ris.as_ref()) {
            Ok(_) => Ok(outcome),
            // 终态报告不再对拍 —— 不是错误。
            Err(crate::compare::pipeline::CompareError::IllegalState(_)) => Ok(outcome),
            Err(crate::compare::pipeline::CompareError::ReportNotFound(_)) => Ok(outcome),
            Err(crate::compare::pipeline::CompareError::Repo(e)) => Err(ProcessError::Repo(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::watcher::CandidateKind;
    use crate::state::idempotency_key;
    use std::path::PathBuf;

    fn candidate(study_no: &str, body: &str) -> Candidate {
        Candidate {
            study_no: study_no.to_string(),
            path: PathBuf::from(format!("inbox/{}/report.txt", study_no)),
            kind: CandidateKind::Txt,
            byte_len: body.len() as u64,
            idempotency_key: idempotency_key(study_no, body),
            raw_bytes: body.as_bytes().to_vec(),
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
    fn 处理一份报告落一条台账行() {
        let repo = Repo::open_in_memory().unwrap();
        let out = IngestProcessor
            .process(&repo, &candidate("S-P1", "病历号：PID-EXAMPLE\n左肾积水 3.2cm"))
            .unwrap();
        assert_eq!(out.items, 1);
        assert_eq!(out.failures, 0);
        assert!(!out.duplicate);
        assert_eq!(count(&repo, "items"), 1);
    }

    #[test]
    fn 重复投递不产生第二条台账行() {
        let repo = Repo::open_in_memory().unwrap();
        let cand = candidate("S-P2", "左肾积水 3.2cm");
        let first = IngestProcessor.process(&repo, &cand).unwrap();
        assert_eq!(first.items, 1);
        let second = IngestProcessor.process(&repo, &cand).unwrap();
        assert!(second.duplicate, "重复投递应被判为幂等命中");
        assert_eq!(count(&repo, "items"), 1, "不得产生第二条台账行");
    }

    #[test]
    fn 编码判不出时显式失败不猜转码() {
        let repo = Repo::open_in_memory().unwrap();
        let mut cand = candidate("S-P3", "左肾积水 3.2cm");
        cand.raw_bytes = vec![0xff, 0xfe, 0x00];
        let out = IngestProcessor.process(&repo, &cand).unwrap();
        assert!(out.undecodable);
        assert!(!out.is_ok());
        assert_eq!(out.items, 0, "编码判不出不得产出条目");
        assert_eq!(count(&repo, "items"), 0, "编码判不出不得落台账行");
        // 这一份不能悄悄消失：落一条带错误码的失败条目。
        assert_eq!(out.failures, 1, "应落 1 条失败条目");
        let (code, status): (String, String) = repo
            .connection()
            .query_row(
                "SELECT audit_log.error_code, reports.status FROM audit_log \
                 JOIN reports ON reports.id = audit_log.report_id \
                 WHERE audit_log.action = 'failed'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(code, "ENCODING_UNKNOWN", "失败要带稳定错误码");
        assert_eq!(status, "parse_failed", "报告状态应为解析失败");
    }

    #[test]
    fn 缺单位的报告落失败条目不落数值() {
        let repo = Repo::open_in_memory().unwrap();
        let out = IngestProcessor
            .process(&repo, &candidate("S-P4", "左肾积水 3.2"))
            .unwrap();
        assert_eq!(out.items, 0);
        assert_eq!(out.failures, 1);
        assert_eq!(count(&repo, "items"), 0);
    }

    /// HL7 v2 候选：守护进程走**适配层**，不把整条报文当自由文本喂内核。
    fn hl7_candidate(study_no: &str, body: &str) -> Candidate {
        Candidate {
            study_no: study_no.to_string(),
            path: PathBuf::from(format!("inbox/{}/report.hl7", study_no)),
            kind: CandidateKind::Hl7,
            byte_len: body.len() as u64,
            idempotency_key: idempotency_key(study_no, body),
            raw_bytes: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn 标准报文候选走适配层取权威字段并落台账行() {
        let repo = Repo::open_in_memory().unwrap();
        let body = "MSH|^~\\&|S|R|RIS|H|202603||ORU^R01|S-H1\r\
                    PID|||PID-EXAMPLE\r\
                    OBR|1||S-H1||||202603\r\
                    OBX|1|TX|KIDNEY^积水^L||左肾积水 3.2cm";
        let out = IngestProcessor
            .process(&repo, &hl7_candidate("S-H1", body))
            .unwrap();
        assert_eq!(out.items, 1, "所见文本应抽出一条");
        assert_eq!(out.failures, 0);
        assert_eq!(count(&repo, "items"), 1);

        // 病历号取自 PID-3（自由文本路径的正则在这里取不到）。
        let patient: String = repo
            .connection()
            .query_row("SELECT patient_id FROM reports", [], |row| row.get(0))
            .unwrap();
        assert_eq!(patient, "PID-EXAMPLE");
    }

    #[test]
    fn 标准报文缺段不落台账行只落失败条目() {
        let repo = Repo::open_in_memory().unwrap();
        let body = "MSH|^~\\&|S|R|RIS|H|202603||ORU^R01|S-H2\r\
                    PID|||PID-EXAMPLE\r\
                    OBR|1||S-H2||||202603";
        let out = IngestProcessor
            .process(&repo, &hl7_candidate("S-H2", body))
            .unwrap();
        assert_eq!(out.items, 0, "缺段不得产出条目");
        assert_eq!(out.failures, 1, "缺段应落 1 条失败条目");
        assert_eq!(count(&repo, "items"), 0, "缺段不得落台账行");
    }
}
