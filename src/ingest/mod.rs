//! 入站适配：投递目录扫描 → 解析 → 落台账。
//!
//! 入站形态是**文件投递**：超声工作站把报告放到 `inbox/<检查号>/report.txt`
//! （或 `report.hl7`）。本模块负责扫目录、读文件、取出病历号、调用解析内核，
//! 再交给 [`crate::state::repo::Repo::commit_ingestion`] 落库。
//!
//! 自由文本的**唯一读取路径**在 [`txt`]：读成 [`txt::SourceDocument`]
//! （原文字节 + 解码文本 + 字节↔字符光标映射）后再喂解析内核 ——
//! 不另开第二条读法，避免「有的路径拿得到字节偏移、有的拿不到」。
//!
//! HL7 v2 `ORU^R01` 的**唯一适配路径**在 [`hl7`]：取权威字段（`OBR-3` 检查号、
//! `PID-3` 病历号、`OBR-7` 检查日期）与 `OBX-5` 所见文本，**汇到同一个解析内核** ——
//! 两条入站通道只有一套抽取口径。
//!
//! 分工边界：**编码探测与解码**在 [`encoding`]：两条通道唯一的解码入口
//! （UTF-8 → GBK → 混编 / 未知），判不出来一律显式失败、**不猜转码**；
//! **目录监听与去抖**在 [`watcher`]：工作站还在写文件时的抢读防护、取件前的幂等预查。

pub mod encoding;
pub mod hl7;
pub mod txt;
pub mod watcher;

use std::path::Path;

use regex::Regex;

use crate::ingest::encoding::decode_report;
use crate::parse::errors::Failure;
use crate::parse::parse_report;
use crate::state::repo::{IngestRecord, Repo, RepoError};
use crate::state::idempotency_key_bytes;
use crate::workflow::concurrency::{
    ConcurrencyError, StudyNoGate, DEFAULT_ATTEMPTS, DEFAULT_LEASE_SECS,
};

/// 病历号字段（免费文本通道）：`病历号：xxx` / `患者ID：xxx`。
///
/// 只取**病历号**，不取姓名 —— 姓名不进台账、不进出站报文。
fn patient_id_pattern() -> Regex {
    Regex::new(r"(?:病历号|患者ID|患者编号|病人号)\s*[:：]\s*([A-Za-z0-9_\-]+)")
        .expect("病历号正则应可编译")
}

/// 从报告正文取出病历号（患者主索引）。
///
/// **唯一口径**：批量入库（[`ingest_inbox`]）与守护调度（[`crate::daemon::processor`]）
/// 都走这条 —— 不各写一份正则，避免两条路径取到的病历号不一样。
pub fn patient_id_of(body: &str) -> Option<String> {
    patient_id_pattern()
        .captures(body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

/// 入站错误。
#[derive(Debug)]
pub enum IngestError {
    /// 投递目录读不到。
    Io(std::io::Error),
    /// 状态库错误。
    Repo(RepoError),
    /// 报告文件读取 / 解码错误。
    Txt(txt::TxtError),
    /// HL7 v2 报告读取 / 解码错误。
    Hl7(hl7::Hl7Error),
    /// **并发冲突**：同一检查号正在被别处处理（守护进程或另一趟批处理）。
    Concurrency(ConcurrencyError),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::Io(e) => write!(f, "投递目录错误：{}", e),
            IngestError::Repo(e) => write!(f, "{}", e),
            IngestError::Txt(e) => write!(f, "{}", e),
            IngestError::Hl7(e) => write!(f, "{}", e),
            IngestError::Concurrency(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for IngestError {}

impl From<std::io::Error> for IngestError {
    fn from(e: std::io::Error) -> Self {
        IngestError::Io(e)
    }
}

impl From<RepoError> for IngestError {
    fn from(e: RepoError) -> Self {
        IngestError::Repo(e)
    }
}

impl From<txt::TxtError> for IngestError {
    fn from(e: txt::TxtError) -> Self {
        IngestError::Txt(e)
    }
}

impl From<hl7::Hl7Error> for IngestError {
    fn from(e: hl7::Hl7Error) -> Self {
        IngestError::Hl7(e)
    }
}

impl From<ConcurrencyError> for IngestError {
    fn from(e: ConcurrencyError) -> Self {
        IngestError::Concurrency(e)
    }
}

/// 一次批次投递的处理汇总。
#[derive(Clone, Debug, Default)]
pub struct IngestSummary {
    /// 成功入库的报告份数。
    pub reports: usize,
    /// 入库的结构化条目数。
    pub items: usize,
    /// 失败条目数（进失败清单）。
    pub failures: usize,
    /// 幂等命中被跳过的份数（重复投递）。
    pub duplicates: usize,
    /// 跳过的投递目录（既没有 `report.txt` 也没有 `report.hl7`）。
    pub skipped: usize,
    /// **编码判不出**的份数（混编 / 非文本：显式失败，**不猜转码**）。
    ///
    /// 这些份数**不产出条目**，但会落一条带错误码的失败条目 ——
    /// 运维在失败清单里看得见「哪一份没解析出来、为什么」。
    pub encoding_failed: usize,
}

/// 处理一个投递目录批次。
///
/// 每个子目录视为一份报告：`<检查号>/report.txt`（自由文本）或 `<检查号>/report.hl7`
/// （HL7 v2 `ORU^R01`）。两条通道**汇到同一个解析内核** —— 抽取口径只有一套；
/// **解码也走同一个入口**（[`decode_report`]）—— 编码判定口径只有一套。
/// **重复投递不产生第二条台账行** —— 幂等键命中即跳过并计入 `duplicates`。
pub fn ingest_inbox(repo: &Repo, inbox: &Path) -> Result<IngestSummary, IngestError> {
    // **词典作用域**：这一批用**同一代**词典 ——
    // 批次跑到一半换了口径，这一批的台账行就会一半旧版本一半新版本，
    // 事后对不上「这批是按哪套口径统计的」。
    let _scope = crate::coerce::dictionary_lifecycle::scope_current();
    let mut summary = IngestSummary::default();
    let dirs = list_report_dirs(inbox)?;

    for dir in dirs {
        // **读一份**：解码 + 解析 + 强转（**还没落库**）。
        //
        // 读入与「落不落」分开，是为了让批量入库（[`ingest_inbox`]）、
        // 离线重放（[`crate::cli::replay`]）与演练（`--dry-run`）
        // 走**同一条**读入路径 —— 只有落库策略不同，口径只有一套。
        let scanned = match read_report_dir(&dir)? {
            Some(scanned) => scanned,
            None => {
                summary.skipped += 1;
                continue;
            }
        };

        // **库级串行**：同一检查号不并发处理。
        //
        // 这一趟批处理与守护进程**可能同时在跑**（运维在 crontab 里加了一趟
        // 批量补处理，守护进程也没停）—— 进程内那份在途集合挡不住另一个进程。
        // 锁在库里，跨进程也成立；抢不到就退避重试，仍抢不到就把这一份
        // **显式报出去**（不当成处理成功，也不静默丢弃）。
        //
        // 锁的键取**投递目录名**（投递身份）：工作站重发、人工重投落回同一个目录。
        let gate = StudyNoGate::new(repo);
        let _lease = gate.acquire(
            &scanned.dir_study_no,
            DEFAULT_LEASE_SECS,
            DEFAULT_ATTEMPTS,
        )?;

        // 编码判不出来的那一份：**不产出条目**，但落一条带错误码的失败条目 ——
        // 这一份不能悄悄消失（`scanned.failures` 里已经带上了）。
        if scanned.encoding_failed {
            summary.encoding_failed += 1;
        }

        let record = IngestRecord {
            study_no: &scanned.study_no,
            patient_id: scanned.patient_id.as_deref(),
            source_path: Some(&scanned.source_path),
            idempotency_key: &scanned.idempotency_key,
            dictionary_version: scanned.dictionary_version.clone(),
            items: &scanned.items,
            failures: &scanned.failures,
        };

        // 走**幂等闸**：查重、跳过、留痕三件事在闸里做完，
        // 调用方拿到的就是结论（「已落库」还是「被幂等跳过」）。
        let guard = crate::workflow::idempotency::IdempotencyGuard::new(repo);
        match guard.commit_ingestion(&record) {
            Ok(outcome) if outcome.is_committed() => {
                summary.reports += 1;
                summary.items += outcome.item_count();
                summary.failures += outcome.failures();
            }
            Ok(outcome) if outcome.is_duplicate() => {
                // 重复投递：幂等键命中，跳过（不产生第二条台账行，已留痕）。
                summary.duplicates += 1;
            }
            Ok(_) => unreachable!("幂等闸只返回已受理或被跳过两种结论"),
            Err(e) => return Err(IngestError::Repo(e)),
        }
    }

    Ok(summary)
}

/// 一份投递报告的**读入结果**（已解码、已解析、已强转，**还没落库**）。
///
/// 这条结构是「读入」与「落库」的**分界线**：
/// 批量入库（[`ingest_inbox`]）、离线重放（`replay`）与演练（`--dry-run`）
/// 读的都是它 —— **只有落库策略不同**（受理 / 跳过 / 刷新），
/// 抽取与强转的口径只有一套。
#[derive(Clone, Debug)]
pub struct ScannedReport {
    /// **投递身份**：投递目录名。工作站重发、人工重投都落回同一个目录，
    /// 库级检查号锁用的就是它。
    pub dir_study_no: String,
    /// **检查号**（自由文本 = 目录名；HL7 = `OBR-3`）。
    pub study_no: String,
    /// 患者主索引（自由文本从正文头部取；HL7 从 `PID-3` 取）。
    pub patient_id: Option<String>,
    /// 来源文件路径。
    pub source_path: String,
    /// 幂等键（`sha256(study_no‖内容字节)`）。
    pub idempotency_key: String,
    /// 产出这些条目时生效的词典版本。
    pub dictionary_version: String,
    /// 结构化条目（台账行的来源）。
    pub items: Vec<crate::parse::extract::ParsedItem>,
    /// 失败条目（带稳定错误码与行列）。
    pub failures: Vec<Failure>,
    /// **编码判不出**（混编 / 非文本）：正文读不出来，
    /// 条目为空、失败条目里带编码错误码。
    pub encoding_failed: bool,
}

/// 读一份投递目录（`<检查号>/report.txt` 或 `<检查号>/report.hl7`）。
///
/// - `Ok(None)` = 这个目录里**没有报告文件**（跳过，不算失败）。
/// - 编码判不出 → **显式失败**：条目为空、失败条目带编码错误码，
///   **不猜转码**（正文都读不出来，凭什么有测值）。
///
/// 两条入站通道（自由文本 / HL7 v2）**汇到同一个解析内核** ——
/// 抽取与强转的口径只有一套；权威字段（检查号 / 病历号）由适配层取。
pub fn read_report_dir(dir: &Path) -> Result<Option<ScannedReport>, IngestError> {
    let dir_study_no = match dir.file_name().and_then(|n| n.to_str()) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => return Ok(None),
    };
    let txt_path = dir.join(txt::REPORT_TXT);
    let hl7_path = dir.join(hl7::REPORT_HL7);

    // 一份报告的来源：自由文本 / HL7 v2 —— 解析都汇到同一个内核。
    let (raw_bytes, source, is_hl7) = if txt_path.is_file() {
        (std::fs::read(&txt_path)?, txt_path, false)
    } else if hl7_path.is_file() {
        (std::fs::read(&hl7_path)?, hl7_path, true)
    } else {
        return Ok(None);
    };

    // 编码判别：**两条通道同一个入口**（UTF-8 → GBK → 混编 / 未知）。
    let decoded = match decode_report(&raw_bytes) {
        Ok(decoded) => decoded,
        Err(e) => {
            // 显式失败：不猜转码、不产出正文与条目，
            // 但落一条带错误码的失败条目 —— 这一份不能悄悄消失。
            let failure = Failure::new(e.error_code(), 0, 0, 0, e.reason())
                .with_detail_code(e.detail_code());
            let key = idempotency_key_bytes(&dir_study_no, &raw_bytes);
            return Ok(Some(ScannedReport {
                dir_study_no: dir_study_no.clone(),
                study_no: dir_study_no.clone(),
                patient_id: None,
                source_path: source.to_string_lossy().to_string(),
                idempotency_key: key,
                dictionary_version: crate::dictionary_version(),
                items: Vec::new(),
                failures: vec![failure],
                encoding_failed: true,
            }));
        }
    };

    let (study_no, patient_id, outcome) = if is_hl7 {
        // HL7 v2 通道：权威字段由适配层取，缺段 / 缺字段落**失败条目**（显式，不静默）。
        let report = hl7::adapt(&decoded.text);
        // 检查号以 **OBR-3** 为准（与 RIS 同源）；OBR 缺失时退回投递目录名，
        // 缺段本身已作为失败条目落进失败清单。
        let study_no = report.study_no.clone().unwrap_or_else(|| dir_study_no.clone());
        (study_no, report.patient_id.clone(), report.outcome)
    } else {
        // 自由文本通道：病历号从正文头部取，抽取走同一个解析内核。
        (
            dir_study_no.clone(),
            patient_id_of(&decoded.text),
            parse_report(&decoded.text),
        )
    };

    // 幂等键按**投递文件的字节**算：与监听侧的预查同源，
    // 任何编码下都算出同一个键（不因解码方式不同而算出两个键）。
    let key = idempotency_key_bytes(&study_no, &raw_bytes);
    Ok(Some(ScannedReport {
        dir_study_no,
        study_no,
        patient_id,
        source_path: source.to_string_lossy().to_string(),
        idempotency_key: key,
        dictionary_version: crate::dictionary_version(),
        items: outcome.items,
        failures: outcome.failures,
        encoding_failed: false,
    }))
}

/// 列出投递目录下的报告子目录（按名字排序，顺序可复现）。
pub fn list_report_dirs(inbox: &Path) -> Result<Vec<std::path::PathBuf>, IngestError> {
    if !inbox.is_dir() {
        return Err(IngestError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("投递目录不存在：{}", inbox.display()),
        )));
    }
    let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(inbox)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    // 排序保证同一批次的处理顺序稳定（台账行顺序可复现）。
    dirs.sort();
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 病历号从报告头部取出() {
        let re = patient_id_pattern();
        let caps = re.captures("病历号：PID-EXAMPLE\n超声所见\n左肾积水 3.2cm").unwrap();
        assert_eq!(caps.get(1).unwrap().as_str(), "PID-EXAMPLE");
    }

    #[test]
    fn 没有病历号时取不到() {
        let re = patient_id_pattern();
        assert!(re.captures("超声所见\n左肾积水 3.2cm").is_none());
    }
}
