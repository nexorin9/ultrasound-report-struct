//! 入站适配：目录轮询监听 + 写入稳定检测（去抖）+ 幂等预查。
//!
//! 守护进程的**事件源**在这里：超声工作站把报告写到 `inbox/<检查号>/report.txt`
//! （或 `report.hl7`）。但工作站写文件**不是原子的** —— 报告可能只写了一半就被看到，
//! 抢读半截正文会得到「3.2」这种没有单位的数值：台账里看起来仍像一个数字，
//! 却少了单位口径，医师还要回头手工核对到底抄漏了没有。
//!
//! 因此本模块**只做发现与稳定判定，不抢读**：
//!
//! 1. **轮询**：1 秒 tick 扫 `inbox/` **下一层目录**，列出 `report.txt` / `report.hl7` 候选
//!    （目录名即检查号，与 [`crate::ingest::txt`] 同一口径）。
//! 2. **写入稳定检测（去抖）**：同一文件**连续两次采样**的**大小与修改时间都不变**，
//!    且两次采样间隔 **≥ 1 秒**，才判定为可取件；仍在增长的文件**本轮跳过并记心跳**。
//! 3. **幂等预查**：取件前用 `study_no‖内容摘要` 查**幂等登记表**；命中即**跳过并写审计**，
//!    **不重复落台账**（工作站重发、人工重投的共同兜底 —— 兜底不靠内存锁，
//!    靠库里那张唯一索引表，进程重启后仍然成立）。
//!
//! **职责边界**：本模块**只发现与稳定判定**；取件之后的排队、并发上限、优雅退出与心跳
//! 由 Task 9 的调度器负责。本模块是**纯同步接口**（时钟可注入），不启动线程、不 sleep、
//! 不常驻 —— 事件驱动守护形态的「常驻」在调度器那一层。
//!
//! **摘要口径**：预查用的幂等键 = `sha256(study_no‖报告内容)`（[`crate::state::idempotency_key_bytes`]），
//! 与入库路径 `crate::state::idempotency_key` **同源**：UTF-8 报告的原始字节与解码正文
//! 逐字节相同，两条路径算出的键必然一致（单测里有交叉断言）。
//! HL7 通道目前按**文件字节**算摘要；Task 10 的 HL7 正文口径若与此不同，
//! 重复投递最终仍由幂等表的唯一索引兜底（只是多一条审计），不会产生第二条台账行。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::log::Logger;
use crate::state::idempotency_key_bytes;
use crate::state::repo::{Repo, RepoError};

/// 投递目录里的自由文本报告文件名。
pub const REPORT_TXT: &str = "report.txt";
/// 投递目录里的 HL7 v2 报告文件名。
pub const REPORT_HL7: &str = "report.hl7";

/// 默认 tick 间隔：1 秒（事件到达延迟 ≤1 秒）。
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(1);
/// 写入稳定判定所需的最小静默时长：1 秒。
pub const DEFAULT_MIN_STABLE: Duration = Duration::from_secs(1);

/// 候选文件类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateKind {
    /// 自由文本报告（`report.txt`）。
    Txt,
    /// HL7 v2 报告（`report.hl7`）。
    Hl7,
}

impl CandidateKind {
    /// 投递目录里的文件名。
    pub fn file_name(self) -> &'static str {
        match self {
            CandidateKind::Txt => REPORT_TXT,
            CandidateKind::Hl7 => REPORT_HL7,
        }
    }

    /// 稳定事件码里的类型词。
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateKind::Txt => "txt",
            CandidateKind::Hl7 => "hl7",
        }
    }
}

/// 候选文件在本轮的稳定状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    /// 首次见到 —— 还没有第二次采样，等下一轮。
    FirstSeen,
    /// 还在写入（大小或修改时间在变）—— **本轮跳过**。
    Growing,
    /// 大小没变，但静默时长还不够 —— 再等一轮。
    Settling,
    /// 写入已稳定，可以取件。
    Stable,
}

/// 一份待处理的候选报告。
#[derive(Clone, Debug)]
pub struct Candidate {
    /// 检查号（取自投递目录名）。
    pub study_no: String,
    /// 源文件路径。
    pub path: PathBuf,
    /// 文件类型。
    pub kind: CandidateKind,
    /// 字节数。
    pub byte_len: u64,
    /// 幂等键 = `sha256(study_no‖报告内容)`。
    pub idempotency_key: String,
    /// 读到的**字节快照** —— 下游按这一份字节处理，避免「算摘要的”和「入库的」不是同一份内容。
    pub raw_bytes: Vec<u8>,
}

impl Candidate {
    /// 字节快照的 UTF-8 视图（不是合法 UTF-8 时为 `None`；只看编码请用 [`Candidate::decoded`]）。
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.raw_bytes).ok()
    }

    /// 按**编码判别**解出正文（唯一解码入口，与批量入库同一条路）。
    ///
    /// 判不出编码即显式失败 —— **不猜转码**：猜错转码会让测量值与单位一起乱码，
    /// 而乱码在台账里看起来仍然像一个数字。
    pub fn decoded(&self) -> Result<crate::ingest::encoding::DecodedReport, crate::ingest::encoding::EncodingError> {
        crate::ingest::encoding::decode_report(&self.raw_bytes)
    }
}

/// 一轮轮询的结果。
#[derive(Clone, Debug, Default)]
pub struct WatchOutcome {
    /// 本轮取到的候选（已过稳定判定 **且** 已过幂等预查）。
    pub candidates: Vec<Candidate>,
    /// 仍在写入、本轮跳过的文件数。
    pub growing: usize,
    /// 还在等稳定（首见 / 静默时长不足）的文件数。
    pub pending: usize,
    /// 幂等预查命中（重复投递）的份数。
    pub duplicates: usize,
}

/// 监听错误。
#[derive(Debug)]
pub enum WatchError {
    /// 文件系统错误（投递目录不存在、文件读不到等）。
    Io(std::io::Error),
    /// 状态库错误（幂等预查 / 审计写入）。
    Repo(RepoError),
}

impl std::fmt::Display for WatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatchError::Io(e) => write!(f, "投递目录监听失败：{}", e),
            WatchError::Repo(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for WatchError {}

impl From<std::io::Error> for WatchError {
    fn from(e: std::io::Error) -> Self {
        WatchError::Io(e)
    }
}

impl From<RepoError> for WatchError {
    fn from(e: RepoError) -> Self {
        WatchError::Repo(e)
    }
}

/// 文件指纹：大小 + 修改时间。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    size: u64,
    mtime: Option<SystemTime>,
}

/// 一个候选文件的上一次采样。
#[derive(Clone, Copy, Debug)]
struct Sample {
    fingerprint: Fingerprint,
    observed_at: Instant,
}

/// 目录轮询监听器。
///
/// 状态全在内存里（采样表 + 已交付表），进程重启后重新观测 ——
/// 重启后第一次采样记为「首见」，再等一轮才交付，**不依赖内存状态做正确性保证**：
/// 重复投递最终由幂等表的唯一索引兜底。
pub struct Watcher {
    inbox: PathBuf,
    tick_interval: Duration,
    min_stable: Duration,
    /// 每个候选文件的上一次采样。
    samples: HashMap<PathBuf, Sample>,
    /// 已交付的候选（连同当时的指纹）：内容没变就不重复交付。
    handed_off: HashMap<PathBuf, Fingerprint>,
}

impl Watcher {
    /// 按默认间隔（tick 1 秒 / 静默 1 秒）构造。
    pub fn new(inbox: impl Into<PathBuf>) -> Self {
        Self::with_intervals(inbox, DEFAULT_TICK_INTERVAL, DEFAULT_MIN_STABLE)
    }

    /// 按指定间隔构造（间隔可配：工作站投递慢的科室可把静默时长调大）。
    pub fn with_intervals(
        inbox: impl Into<PathBuf>,
        tick_interval: Duration,
        min_stable: Duration,
    ) -> Self {
        Watcher {
            inbox: inbox.into(),
            tick_interval,
            min_stable,
            samples: HashMap::new(),
            handed_off: HashMap::new(),
        }
    }

    /// 投递目录。
    pub fn inbox(&self) -> &Path {
        &self.inbox
    }

    /// tick 间隔。
    pub fn tick_interval(&self) -> Duration {
        self.tick_interval
    }

    /// 轮询一轮（真实时钟）。
    pub fn poll_once(&mut self, repo: &Repo, logger: &Logger) -> Result<WatchOutcome, WatchError> {
        self.poll_once_at(repo, logger, Instant::now())
    }

    /// 轮询一轮（时钟可注入 —— 测试里不必真的睡 1 秒）。
    ///
    /// 一轮的流程：列目录 → 采样 → 稳定判定 → 幂等预查 → 交付候选。
    pub fn poll_once_at(
        &mut self,
        repo: &Repo,
        logger: &Logger,
        now: Instant,
    ) -> Result<WatchOutcome, WatchError> {
        let mut outcome = WatchOutcome::default();
        let dirs = self.list_study_dirs()?;
        let mut alive: HashSet<PathBuf> = HashSet::new();

        for dir in dirs {
            let study_no = match dir.file_name().and_then(|name| name.to_str()) {
                Some(name) if !name.is_empty() => name.to_string(),
                _ => continue,
            };

            for kind in [CandidateKind::Txt, CandidateKind::Hl7] {
                let path = dir.join(kind.file_name());
                if !path.is_file() {
                    continue;
                }
                alive.insert(path.clone());

                let meta = match std::fs::metadata(&path) {
                    Ok(meta) => meta,
                    Err(_) => continue,
                };
                let fingerprint = Fingerprint {
                    size: meta.len(),
                    mtime: meta.modified().ok(),
                };

                let state = self.classify(&path, fingerprint, now);
                match state {
                    FileState::FirstSeen | FileState::Settling => {
                        outcome.pending += 1;
                    }
                    FileState::Growing => {
                        // 还在写 —— 跳过本轮并记心跳（不抢读半截正文）。
                        outcome.growing += 1;
                        logger.info(
                            "FILE_GROWING",
                            &[
                                ("study_no", study_no.clone()),
                                ("file", kind.file_name().to_string()),
                                ("bytes", fingerprint.size.to_string()),
                            ],
                        );
                    }
                    FileState::Stable => {
                        // 内容没变且已交付过 → 不重复交付。
                        if self.handed_off.get(&path) == Some(&fingerprint) {
                            continue;
                        }
                        // 取件：读字节快照。
                        let raw_bytes = match std::fs::read(&path) {
                            Ok(bytes) => bytes,
                            Err(_) => continue,
                        };
                        if raw_bytes.len() as u64 != fingerprint.size {
                            // 读的时候还在写 —— 视为仍在增长，本轮跳过。
                            outcome.growing += 1;
                            self.samples.insert(
                                path.clone(),
                                Sample {
                                    fingerprint: Fingerprint {
                                        size: raw_bytes.len() as u64,
                                        mtime: meta.modified().ok(),
                                    },
                                    observed_at: now,
                                },
                            );
                            logger.info(
                                "FILE_GROWING",
                                &[
                                    ("study_no", study_no.clone()),
                                    ("file", kind.file_name().to_string()),
                                    ("bytes", raw_bytes.len().to_string()),
                                ],
                            );
                            continue;
                        }

                        let key = idempotency_key_bytes(&study_no, &raw_bytes);

                        // 幂等预查：命中 → 跳过并写审计（不重复落台账）。
                        if let Some(record) = repo.find_idempotency(&key)? {
                            let scope = record.scope.clone();
                            let report_id = repo.find_by_idempotency_key(&key)?.map(|row| row.id);
                            // 留痕走**唯一写入出口**（与落库闸的命中同一条路径）。
                            crate::workflow::idempotency::record_duplicate_skipped(
                                repo,
                                report_id,
                                &study_no,
                                &key,
                                &crate::dictionary_version(),
                                &scope,
                            )?;
                            outcome.duplicates += 1;
                            self.handed_off.insert(path.clone(), fingerprint);
                            logger.info(
                                "DUPLICATE_SKIPPED",
                                &[
                                    ("study_no", study_no.clone()),
                                    ("scope", scope),
                                ],
                            );
                            continue;
                        }

                        outcome.candidates.push(Candidate {
                            study_no: study_no.clone(),
                            path: path.clone(),
                            kind,
                            byte_len: raw_bytes.len() as u64,
                            idempotency_key: key,
                            raw_bytes,
                        });
                        self.handed_off.insert(path.clone(), fingerprint);
                        logger.info(
                            "REPORT_DISCOVERED",
                            &[
                                ("study_no", study_no.clone()),
                                ("kind", kind.as_str().to_string()),
                                ("bytes", fingerprint.size.to_string()),
                            ],
                        );
                    }
                }
            }
        }

        // 文件消失（工作站撤单 / 目录清理）→ 采样与交付记录一并清掉。
        self.samples.retain(|path, _| alive.contains(path));
        self.handed_off.retain(|path, _| alive.contains(path));

        logger.info(
            "WATCH_TICK",
            &[
                ("candidates", outcome.candidates.len().to_string()),
                ("growing", outcome.growing.to_string()),
                ("pending", outcome.pending.to_string()),
                ("duplicates", outcome.duplicates.to_string()),
            ],
        );

        Ok(outcome)
    }

    /// 交还候选（处理失败时由调度器调用，让下一轮重新取件）。
    pub fn release(&mut self, path: &Path) {
        self.handed_off.remove(path);
    }

    /// 投递目录下的检查号目录（排序，保证处理顺序可复现）。
    fn list_study_dirs(&self) -> Result<Vec<PathBuf>, WatchError> {
        let entries = std::fs::read_dir(&self.inbox)?;
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        Ok(dirs)
    }

    /// 稳定判定：把本次采样与上一次比较，给出状态。
    fn classify(&mut self, path: &Path, fingerprint: Fingerprint, now: Instant) -> FileState {
        match self.samples.get(path).copied() {
            None => {
                // 首见：还没有第二次采样，等下一轮。
                self.samples.insert(
                    path.to_path_buf(),
                    Sample {
                        fingerprint,
                        observed_at: now,
                    },
                );
                FileState::FirstSeen
            }
            Some(prev) if prev.fingerprint != fingerprint => {
                // 大小或修改时间变了 —— 还在写，重新计时。
                self.samples.insert(
                    path.to_path_buf(),
                    Sample {
                        fingerprint,
                        observed_at: now,
                    },
                );
                FileState::Growing
            }
            Some(prev) => {
                let quiet = now
                    .checked_duration_since(prev.observed_at)
                    .unwrap_or(Duration::ZERO);
                if quiet < self.min_stable {
                    // 大小没变但静默时长不够：继续等。
                    // **静默窗口连续累积** —— 这里不动 `observed_at`：
                    // 它记的是「上一次内容发生变化（或首见）的时间」，
                    // 一旦重置就永远等不到稳定。
                    FileState::Settling
                } else {
                    FileState::Stable
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_report;
    use crate::state::repo::IngestRecord;
    use crate::state::{idempotency_key, idempotency_key_bytes};
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// 收集日志行的内存缓冲（测试替身，仅测试数据层）。
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("urs-watcher-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_report(inbox: &Path, study_no: &str, body: &str) -> PathBuf {
        let dir = inbox.join(study_no);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(REPORT_TXT), body).unwrap();
        dir
    }

    fn watcher(inbox: &Path) -> Watcher {
        Watcher::with_intervals(inbox, Duration::from_secs(1), Duration::from_secs(1))
    }

    /// 把一份报告按正常入库路径登记进去（模拟「这份已经处理过」）。
    fn commit_once(repo: &Repo, study_no: &str, body: &str) {
        let outcome = parse_report(body);
        repo.commit_ingestion(&IngestRecord {
            study_no,
            patient_id: None,
            source_path: None,
            idempotency_key: &idempotency_key(study_no, body),
            dictionary_version: crate::dictionary_version(),
            items: &outcome.items,
            failures: &outcome.failures,
        })
        .unwrap();
    }

    fn count(repo: &Repo, table: &str) -> i64 {
        repo.connection()
            .query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn 半写文件不被抢读稳定后才取出() {
        let inbox = temp_dir("growing");
        let dir = write_report(&inbox, "S-GROW", "左肾积水 3.2");
        let repo = Repo::open_in_memory().unwrap();
        let buffer = Buffer::default();
        let logger = Logger::with_sink(Box::new(buffer.clone()));
        let mut watcher = watcher(&inbox);
        let t0 = Instant::now();

        // 第 1 轮：首见，还在等第二次采样。
        let out = watcher.poll_once_at(&repo, &logger, t0).unwrap();
        assert!(out.candidates.is_empty(), "首见不得交付");
        assert_eq!(out.pending, 1);
        assert_eq!(out.growing, 0);

        // 工作站继续写（正文写了一半）。
        let full = "左肾积水 3.2cm";
        std::fs::write(dir.join(REPORT_TXT), full).unwrap();

        // 第 2 轮：大小变了 —— 仍在写，本轮跳过并记心跳。
        let out = watcher
            .poll_once_at(&repo, &logger, t0 + Duration::from_secs(1))
            .unwrap();
        assert!(out.candidates.is_empty(), "仍在写入的文件不得被抢读");
        assert_eq!(out.growing, 1);
        assert!(buffer.text().contains("FILE_GROWING"), "{}", buffer.text());

        // 第 3 轮：大小不再变化且已静默 ≥1 秒 → 交付，且读到的是**完整**内容。
        let out = watcher
            .poll_once_at(&repo, &logger, t0 + Duration::from_secs(2))
            .unwrap();
        assert_eq!(out.candidates.len(), 1, "稳定后应取出");
        assert_eq!(out.candidates[0].study_no, "S-GROW");
        assert_eq!(out.candidates[0].byte_len, full.len() as u64);
        assert_eq!(out.candidates[0].text(), Some(full));
        assert_eq!(out.candidates[0].kind, CandidateKind::Txt);

        std::fs::remove_dir_all(&inbox).ok();
    }

    #[test]
    fn 静默时长不足时继续等待() {
        let inbox = temp_dir("settling");
        write_report(&inbox, "S-SETTLE", "左肾积水 3.2cm");
        let repo = Repo::open_in_memory().unwrap();
        let buffer = Buffer::default();
        let logger = Logger::with_sink(Box::new(buffer.clone()));
        // 静默窗口要求 3 秒，tick 1 秒。
        let mut watcher =
            Watcher::with_intervals(&inbox, Duration::from_secs(1), Duration::from_secs(3));
        let t0 = Instant::now();

        watcher.poll_once_at(&repo, &logger, t0).unwrap();
        // 第 2 轮：静默才 1 秒 —— 继续等。
        let out = watcher
            .poll_once_at(&repo, &logger, t0 + Duration::from_secs(1))
            .unwrap();
        assert!(out.candidates.is_empty());
        assert_eq!(out.pending, 1);
        // 第 3 轮：静默 2 秒 —— 仍不够。
        let out = watcher
            .poll_once_at(&repo, &logger, t0 + Duration::from_secs(2))
            .unwrap();
        assert!(out.candidates.is_empty());
        assert_eq!(out.pending, 1);
        // 第 4 轮：静默 3 秒 —— 可取件。
        let out = watcher
            .poll_once_at(&repo, &logger, t0 + Duration::from_secs(3))
            .unwrap();
        assert_eq!(out.candidates.len(), 1);

        std::fs::remove_dir_all(&inbox).ok();
    }

    #[test]
    fn 稳定文件只交付一次不重复交付() {
        let inbox = temp_dir("once");
        write_report(&inbox, "S-ONCE", "左肾积水 3.2cm");
        let repo = Repo::open_in_memory().unwrap();
        let buffer = Buffer::default();
        let logger = Logger::with_sink(Box::new(buffer.clone()));
        let mut watcher = watcher(&inbox);
        let t0 = Instant::now();

        watcher.poll_once_at(&repo, &logger, t0).unwrap();
        let out = watcher
            .poll_once_at(&repo, &logger, t0 + Duration::from_secs(1))
            .unwrap();
        assert_eq!(out.candidates.len(), 1);

        // 后续轮次：内容没变 —— 不重复交付。
        for step in 2..5 {
            let out = watcher
                .poll_once_at(&repo, &logger, t0 + Duration::from_secs(step))
                .unwrap();
            assert!(out.candidates.is_empty(), "同一份内容不得重复交付");
        }

        std::fs::remove_dir_all(&inbox).ok();
    }

    #[test]
    fn 重复投递命中幂等预查跳过且不产生第二条台账行() {
        let inbox = temp_dir("dup");
        let body = "左肾积水 3.2cm";
        write_report(&inbox, "S-DUP", body);
        let repo = Repo::open_in_memory().unwrap();

        // 先按正常入库路径登记一次（这份报告「已经处理过」）。
        commit_once(&repo, "S-DUP", body);
        let items_before = count(&repo, "items");
        let reports_before = count(&repo, "reports");
        assert_eq!(items_before, 1);

        let buffer = Buffer::default();
        let logger = Logger::with_sink(Box::new(buffer.clone()));
        let mut watcher = watcher(&inbox);
        let t0 = Instant::now();

        watcher.poll_once_at(&repo, &logger, t0).unwrap();
        let out = watcher
            .poll_once_at(&repo, &logger, t0 + Duration::from_secs(1))
            .unwrap();

        assert!(out.candidates.is_empty(), "命中幂等预查的投递不得再交付");
        assert_eq!(out.duplicates, 1);
        assert_eq!(count(&repo, "items"), items_before, "不得产生第二条台账行");
        assert_eq!(count(&repo, "reports"), reports_before);

        // 审计留痕：动作 = duplicate_skipped，带输入摘要哈希。
        let (action, digest): (String, String) = repo
            .connection()
            .query_row(
                "SELECT action, input_digest FROM audit_log WHERE action = 'duplicate_skipped'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(action, "duplicate_skipped");
        assert_eq!(digest, idempotency_key_bytes("S-DUP", body.as_bytes()));
        assert!(buffer.text().contains("DUPLICATE_SKIPPED"), "{}", buffer.text());

        std::fs::remove_dir_all(&inbox).ok();
    }

    #[test]
    fn 预查摘要与入库路径幂等键同源() {
        let body = "病历号：PID-EXAMPLE\n超声所见\n左肾积水 3.2cm\n";
        assert_eq!(
            idempotency_key_bytes("S-KEY", body.as_bytes()),
            idempotency_key("S-KEY", body),
            "预查用的摘要与入库路径的幂等键必须同源，否则预查永远不命中"
        );
    }

    #[test]
    fn hl7候选同样被列出() {
        let inbox = temp_dir("hl7");
        let dir = inbox.join("S-HL7");
        std::fs::create_dir_all(&dir).unwrap();
        let hl7 = "MSH|^~\\&|US|ULTRA|OBJ|1|P|20261031||ORU^R01\rPID|1||PID-EXAMPLE\r";
        std::fs::write(dir.join(REPORT_HL7), hl7).unwrap();

        let repo = Repo::open_in_memory().unwrap();
        let buffer = Buffer::default();
        let logger = Logger::with_sink(Box::new(buffer.clone()));
        let mut watcher = watcher(&inbox);
        let t0 = Instant::now();

        watcher.poll_once_at(&repo, &logger, t0).unwrap();
        let out = watcher
            .poll_once_at(&repo, &logger, t0 + Duration::from_secs(1))
            .unwrap();
        assert_eq!(out.candidates.len(), 1);
        assert_eq!(out.candidates[0].kind, CandidateKind::Hl7);
        assert_eq!(out.candidates[0].byte_len, hl7.len() as u64);

        std::fs::remove_dir_all(&inbox).ok();
    }

    #[test]
    fn 投递目录不存在时显式报错() {
        let repo = Repo::open_in_memory().unwrap();
        let buffer = Buffer::default();
        let logger = Logger::with_sink(Box::new(buffer.clone()));
        let mut watcher = Watcher::new("/nonexistent-urs-inbox-path");
        let err = watcher.poll_once_at(&repo, &logger, Instant::now());
        match err {
            Err(WatchError::Io(_)) => {}
            other => panic!("投递目录不存在应显式报错，实际：{:?}", other),
        }
    }

    #[test]
    fn 空投递目录不报错也不交付() {
        let inbox = temp_dir("empty");
        let repo = Repo::open_in_memory().unwrap();
        let buffer = Buffer::default();
        let logger = Logger::with_sink(Box::new(buffer.clone()));
        let mut watcher = watcher(&inbox);
        let out = watcher.poll_once_at(&repo, &logger, Instant::now()).unwrap();
        assert!(out.candidates.is_empty());
        assert_eq!(out.pending, 0);
        assert_eq!(out.growing, 0);
        assert_eq!(out.duplicates, 0);
        std::fs::remove_dir_all(&inbox).ok();
    }

    #[test]
    fn 交还候选后下一轮重新交付() {
        let inbox = temp_dir("release");
        write_report(&inbox, "S-REL", "左肾积水 3.2cm");
        let repo = Repo::open_in_memory().unwrap();
        let buffer = Buffer::default();
        let logger = Logger::with_sink(Box::new(buffer.clone()));
        let mut watcher = watcher(&inbox);
        let t0 = Instant::now();

        watcher.poll_once_at(&repo, &logger, t0).unwrap();
        let out = watcher
            .poll_once_at(&repo, &logger, t0 + Duration::from_secs(1))
            .unwrap();
        assert_eq!(out.candidates.len(), 1);
        let path = out.candidates[0].path.clone();

        // 调度器处理失败 → 交还，下一轮重新取件。
        watcher.release(&path);
        let out = watcher
            .poll_once_at(&repo, &logger, t0 + Duration::from_secs(2))
            .unwrap();
        assert_eq!(out.candidates.len(), 1, "交还后应重新交付");

        std::fs::remove_dir_all(&inbox).ok();
    }
}
