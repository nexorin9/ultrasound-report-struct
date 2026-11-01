//! 守护调度器：任务队列 + 并发上限 + 优雅退出 + 心跳。
//!
//! **这一层是「事件驱动守护」形态的落点**：进程常驻，事件到达即处理，
//! 不退回「跑一次出一次表」的批处理脚本。
//!
//! ## 一轮是什么
//!
//! ```text
//! ┌─ 调度线程 ──────────────────────────────┐   ┌─ 工作线程 × N ─────────────┐
//! │ 轮询事件源 → 稳定判定 → 幂等预查        │   │ 取任务 → 解析 → 强转 → 落库 │
//! │        ↓ 候选入队                       │──▶│        ↑ 处理完释放在途标记  │
//! │ 心跳（processed/failed/待确认/待回写）  │   └────────────────────────────┘
//! │ 等一个 tick（停机信号 50ms 内可见）      │
//! └─────────────────────────────────────────┘
//! ```
//!
//! ## 三条不变量
//!
//! 1. **并发有上限**：同时在处理的任务数 ≤ `max_concurrency`（默认 4）。
//!    8 份报告同时到达，也只会有 4 份在处理 —— 超声科机房的机器带不动无限并发，
//!    库写入也会被拖慢。
//! 2. **同一检查号串行**：调度器在**派发层**不并发派发同一 `study_no`；
//!    但**正确性不依赖这份内存态** —— 幂等键的唯一索引在**数据库层**兜底，
//!    进程重启后依然成立（不产生第二条台账行）。库级串行化见 Task 36。
//! 3. **停机不丢在途**：收到 SIGTERM / SIGINT → 停止取新任务 → 等在途任务结束
//!    → 落库 → 退出码 0。排队未开始的任务被丢弃，但**源文件仍在投递目录里**，
//!    重启后会被重新发现。

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::ingest::watcher::{Candidate, Watcher};
use crate::log::Logger;
use crate::state::repo::Repo;

use super::processor::ReportProcessor;

/// 停机检查的轮询粒度（毫秒）—— 信号到停机的最大延迟。
const STOP_POLL_MS: u64 = 50;

/// 默认并发上限。
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;

/// 守护配置。
#[derive(Clone, Debug)]
pub struct DaemonConfig {
    /// 超声工作站投递目录。
    pub inbox: PathBuf,
    /// 状态库路径（每个工作线程各开一条连接）。
    pub state_db: PathBuf,
    /// 并发上限（同一检查号串行，不同检查号并行）。
    pub max_concurrency: usize,
    /// 目录轮询间隔（事件到达延迟 ≤ 该值）。
    pub tick_interval: Duration,
    /// 写入稳定判定窗口（文件大小与修改时间连续不变 ≥ 该时长才取件）。
    pub min_stable: Duration,
    /// 心跳间隔（每 N 轮打一次；0 = 不打心跳）。
    pub heartbeat_every_ticks: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            inbox: PathBuf::from("inbox"),
            state_db: PathBuf::from("data/state.sqlite"),
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            tick_interval: Duration::from_millis(1000),
            min_stable: Duration::from_millis(1000),
            heartbeat_every_ticks: 1,
        }
    }
}

/// 调度错误。
#[derive(Debug)]
pub enum SchedulerError {
    /// 状态库错误。
    Repo(crate::state::repo::RepoError),
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchedulerError::Repo(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for SchedulerError {}

/// 调度指标（心跳与运维观测用）。
#[derive(Debug, Default)]
pub struct SchedulerMetrics {
    /// 已处理报告数。
    pub processed: AtomicU64,
    /// 失败条目数（含读不出正文的报告各计 1）。
    pub failed: AtomicU64,
    /// 幂等命中被跳过的份数（重复投递）。
    pub duplicates: AtomicU64,
    /// 读不出正文的份数。
    pub undecodable: AtomicU64,
    /// 处理过程中出错（状态库错误等）的次数。
    pub errors: AtomicU64,
    /// 当前在途任务数。
    pub in_flight: AtomicUsize,
    /// **并发峰值**（观测到的同时在处理任务数最大值）。
    pub peak_concurrency: AtomicUsize,
}

/// 队列状态（队列 + 在途集合 + 停机标志）。
#[derive(Default)]
struct QueueState {
    /// 待处理任务（派发顺序）。
    ready: VecDeque<Candidate>,
    /// 被推迟的任务（同一检查号在途，等前面的处理完再领）。
    deferred: VecDeque<Candidate>,
    /// 在途的检查号（同一检查号不并发处理）。
    in_flight: HashSet<String>,
    /// 停机标志：置上后不再派发新任务。
    stop: bool,
}

/// 任务队列（互斥量 + 条件变量）。
struct Queue {
    state: Mutex<QueueState>,
    cv: Condvar,
}

/// 守护调度器。
pub struct Scheduler {
    cfg: DaemonConfig,
    logger: Arc<Logger>,
    processor: Arc<dyn ReportProcessor>,
    queue: Arc<Queue>,
    watcher: Arc<Mutex<Watcher>>,
    shutdown: Arc<AtomicBool>,
    stop_reason: Mutex<String>,
    metrics: Arc<SchedulerMetrics>,
}

impl Scheduler {
    /// 按配置构造调度器（处理器可替换 —— 测试用慢处理器观测并发峰值）。
    pub fn new(
        cfg: DaemonConfig,
        processor: Arc<dyn ReportProcessor>,
        logger: Arc<Logger>,
    ) -> Scheduler {
        Scheduler {
            cfg,
            logger,
            processor,
            queue: Arc::new(Queue {
                state: Mutex::new(QueueState::default()),
                cv: Condvar::new(),
            }),
            watcher: Arc::new(Mutex::new(Watcher::new("inbox"))),
            shutdown: Arc::new(AtomicBool::new(false)),
            stop_reason: Mutex::new(String::from("未收到停机请求")),
            metrics: Arc::new(SchedulerMetrics::default()),
        }
    }

    /// 停机标志（供外部把信号接到调度器上）。
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    /// 指标句柄（观测并发峰值与处理计数）。
    pub fn metrics(&self) -> Arc<SchedulerMetrics> {
        Arc::clone(&self.metrics)
    }

    /// 请求停机（等价于收到 SIGTERM / SIGINT）。
    pub fn request_stop(&self) {
        self.request_stop_with_reason("外部请求停机");
    }

    /// 请求停机并记录原因（进日志，值班时能看出是谁停的）。
    pub fn request_stop_with_reason(&self, reason: &str) {
        let mut slot = self.stop_reason.lock().unwrap_or_else(|e| e.into_inner());
        if *slot == "未收到停机请求" {
            *slot = reason.to_string();
        }
        drop(slot);
        self.shutdown.store(true, Ordering::SeqCst);
        // 唤醒等待中的调度线程与工作线程（他们最多 50ms 后也会自己看见标志）。
        self.queue.cv.notify_all();
    }

    /// 常驻运行，直到收到停机请求。
    pub fn run(&self) -> Result<(), SchedulerError> {
        // 1) 起工作线程（每个线程自己开一条 SQLite 连接 —— WAL 模式支持多连接）。
        let worker_count = self.cfg.max_concurrency.max(1);
        let mut workers: Vec<JoinHandle<()>> = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let repo = Repo::open(&self.cfg.state_db).map_err(SchedulerError::Repo)?;
            workers.push(self.spawn_worker(repo));
        }

        // 2) 调度线程自己的连接（事件源的幂等预查与审计写入用）。
        let repo = Repo::open(&self.cfg.state_db).map_err(SchedulerError::Repo)?;
        {
            let mut watcher = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
            *watcher = Watcher::with_intervals(
                &self.cfg.inbox,
                self.cfg.tick_interval,
                self.cfg.min_stable,
            );
        }

        self.logger.info(
            "DAEMON_STARTED",
            &[
                ("inbox", self.cfg.inbox.display().to_string()),
                ("state_db", self.cfg.state_db.display().to_string()),
                ("max_concurrency", worker_count.to_string()),
                ("tick_ms", self.cfg.tick_interval.as_millis().to_string()),
            ],
        );

        // 3) 调度循环。
        let mut tick: u64 = 0;
        loop {
            if self.should_stop() {
                break;
            }

            // 3.0) 词典热更新：运维改了词典文件就换一代口径。
            //     **不打断在途任务** —— 在途任务靠作用域固定住自己那一代，
            //     新任务用新的一代；台账行各自留着自己那代的版本号。
            match crate::coerce::dictionary_lifecycle::reload_current_if_changed() {
                Ok(Some(outcome)) => {
                    self.logger.info(
                        "DICT_RELOADED",
                        &[
                            ("generation", outcome.generation.to_string()),
                            ("previous_version", outcome.previous_version.clone()),
                            ("version", outcome.current_version.clone()),
                            ("retired_holders", outcome.retired_holders.to_string()),
                        ],
                    );
                }
                Ok(None) => {}
                Err(err) => {
                    // 词典坏了：**不换口径**（继续用旧的一代），但要显式记事件 ——
                    // 值班的人得知道运维改坏了词典。
                    self.metrics.errors.fetch_add(1, Ordering::SeqCst);
                    self.logger.error(
                        "DICT_RELOAD_FAILED",
                        &[("error_code", err.code().to_string()), ("error", err.message())],
                    );
                }
            }

            // 3.1) 轮询一轮：发现候选 + 写入稳定判定 + 幂等预查。
            let poll = {
                let mut watcher = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
                watcher.poll_once(&repo, &self.logger)
            };
            match poll {
                Ok(outcome) => {
                    if !outcome.candidates.is_empty() {
                        self.enqueue(outcome.candidates);
                    }
                }
                Err(e) => {
                    // 投递目录不存在 / 状态库出错：**显式记事件**，不静默吞掉。
                    self.metrics.errors.fetch_add(1, Ordering::SeqCst);
                    self.logger.error("WATCH_FAILED", &[("error", e.to_string())]);
                }
            }

            // 3.2) 心跳：回答「昨天有多少报告没解析出来」。
            tick += 1;
            if self.cfg.heartbeat_every_ticks > 0 && tick % self.cfg.heartbeat_every_ticks == 0 {
                self.heartbeat(&repo, tick);
            }

            // 3.3) 等一个 tick（停机请求在 50ms 内被看见）。
            if self.wait_tick() {
                break;
            }
        }

        // 4) 优雅退出：停止取新任务 → 等在途任务结束 → 落库 → 退出码 0。
        let reason = self
            .stop_reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.logger.info("DAEMON_STOPPING", &[("reason", reason)]);
        self.stop_workers();
        for worker in workers {
            if worker.join().is_err() {
                self.logger.error("WORKER_PANICKED", &[]);
                self.metrics.errors.fetch_add(1, Ordering::SeqCst);
            }
        }
        self.logger.info(
            "DAEMON_STOPPED",
            &[
                ("processed", self.metrics.processed.load(Ordering::SeqCst).to_string()),
                ("failed", self.metrics.failed.load(Ordering::SeqCst).to_string()),
                ("duplicates", self.metrics.duplicates.load(Ordering::SeqCst).to_string()),
                (
                    "peak_concurrency",
                    self.metrics.peak_concurrency.load(Ordering::SeqCst).to_string(),
                ),
            ],
        );
        Ok(())
    }

    /// 是否已请求停机。
    fn should_stop(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// 候选入队。
    fn enqueue(&self, candidates: Vec<Candidate>) {
        {
            let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
            for candidate in candidates {
                state.ready.push_back(candidate);
            }
        }
        self.queue.cv.notify_all();
    }

    /// 停止派发新任务（排队未开始的丢弃；源文件仍在投递目录里，重启后重新发现）。
    fn stop_workers(&self) {
        {
            let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stop = true;
            state.ready.clear();
            state.deferred.clear();
        }
        self.queue.cv.notify_all();
    }

    /// 等一个 tick；返回 `true` 表示期间收到停机请求。
    fn wait_tick(&self) -> bool {
        let deadline = Instant::now() + self.cfg.tick_interval;
        let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        while !self.should_stop() {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let remain = deadline - now;
            let wait = remain.min(Duration::from_millis(STOP_POLL_MS));
            let (guard, _) = self
                .queue
                .cv
                .wait_timeout(state, wait)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
        }
        true
    }

    /// 心跳：处理数 / 失败数 / 待确认数 / 待回写数 / 最近事件时间。
    fn heartbeat(&self, repo: &Repo, tick: u64) {
        let counts = repo.count_by_status().unwrap_or_default();
        let awaiting_review: i64 = counts
            .iter()
            .filter(|(status, _)| {
                matches!(status.as_str(), "received" | "parsed" | "awaiting_review")
            })
            .map(|(_, count)| *count)
            .sum();
        let pending_writeback: i64 = counts
            .iter()
            .filter(|(status, _)| status == "pending_writeback")
            .map(|(_, count)| *count)
            .sum();
        let last_event_at = repo
            .last_event_at()
            .ok()
            .flatten()
            .unwrap_or_else(|| "-".to_string());

        let mut fields: Vec<(&str, String)> = vec![
            ("tick", tick.to_string()),
            ("processed", self.metrics.processed.load(Ordering::SeqCst).to_string()),
            ("failed", self.metrics.failed.load(Ordering::SeqCst).to_string()),
            ("failed_rate", format!("{:.3}", self.failed_rate())),
            ("awaiting_review", awaiting_review.to_string()),
            ("pending_writeback", pending_writeback.to_string()),
            ("last_event_at", last_event_at),
            ("in_flight", self.metrics.in_flight.load(Ordering::SeqCst).to_string()),
            (
                "peak_concurrency",
                self.metrics.peak_concurrency.load(Ordering::SeqCst).to_string(),
            ),
            ("duplicates", self.metrics.duplicates.load(Ordering::SeqCst).to_string()),
            ("errors", self.metrics.errors.load(Ordering::SeqCst).to_string()),
        ];

        // **库口径**的当日运行摘要（`summary_*` 前缀）。
        //
        // 上面那组字段是**进程内**计数（这次运行处理了多少份），重启即归零；
        // 这一组是从**状态库**算的，跨重启、跨进程都还在 ——
        // 运维问的是「昨天有多少报告没解析出来」，那个数不能因为今早重启过一次就查不到。
        // 失败条目按错误码聚合，值班的人一眼能分清「编码问题」还是「词典缺口」。
        if let Ok(summary) = crate::workflow::summary::summarize(
            repo,
            &crate::workflow::summary::SummaryWindow::today(),
        ) {
            fields.push(("summary_date", summary.window_label.clone()));
            fields.push(("summary_processed", summary.processed.to_string()));
            fields.push(("summary_failed", summary.failed.to_string()));
            fields.push(("summary_failed_rate", format!("{:.3}", summary.failed_rate())));
            fields.push(("summary_failed_items", summary.failed_items.to_string()));
            fields.push(("summary_duplicates", summary.duplicates.to_string()));
            fields.push((
                "summary_awaiting_review",
                summary.awaiting_review.to_string(),
            ));
            fields.push((
                "summary_pending_writeback",
                summary.pending_writeback.to_string(),
            ));
            fields.push(("summary_error_codes", summary.render_error_codes()));
        }

        self.logger.info("DAEMON_HEARTBEAT", &fields);
    }

    /// 失败率（处理数为 0 时取 0）。
    fn failed_rate(&self) -> f64 {
        let processed = self.metrics.processed.load(Ordering::SeqCst);
        if processed == 0 {
            return 0.0;
        }
        self.metrics.failed.load(Ordering::SeqCst) as f64 / processed as f64
    }

    /// 起一个工作线程。
    fn spawn_worker(&self, repo: Repo) -> JoinHandle<()> {
        let queue = Arc::clone(&self.queue);
        let watcher = Arc::clone(&self.watcher);
        let processor = Arc::clone(&self.processor);
        let logger = Arc::clone(&self.logger);
        let metrics = Arc::clone(&self.metrics);
        let shutdown = Arc::clone(&self.shutdown);

        std::thread::spawn(move || {
            loop {
                // 取任务：**同一把锁内**完成「取件 + 标记在途」，
                // 否则两个 worker 可能同时领走同一检查号的两份内容。
                let candidate = {
                    let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if let Some(candidate) = take_task(&mut state) {
                            break Some(candidate);
                        }
                        if state.stop || shutdown.load(Ordering::SeqCst) {
                            break None;
                        }
                        let (guard, _) = queue
                            .cv
                            .wait_timeout(state, Duration::from_millis(STOP_POLL_MS))
                            .unwrap_or_else(|e| e.into_inner());
                        state = guard;
                    }
                };

                let candidate = match candidate {
                    Some(candidate) => candidate,
                    None => break,
                };

                let study_no = candidate.study_no.clone();
                let current = metrics.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                metrics.peak_concurrency.fetch_max(current, Ordering::SeqCst);

                let result = processor.process(&repo, &candidate);

                metrics.in_flight.fetch_sub(1, Ordering::SeqCst);

                match result {
                    Ok(outcome) => {
                        metrics.processed.fetch_add(1, Ordering::SeqCst);
                        metrics.failed.fetch_add(outcome.failures as u64, Ordering::SeqCst);
                        if outcome.duplicate {
                            metrics.duplicates.fetch_add(1, Ordering::SeqCst);
                            logger.info(
                                "REPORT_DUPLICATE",
                                &[("study_no", study_no.clone())],
                            );
                        } else if outcome.undecodable {
                            metrics.undecodable.fetch_add(1, Ordering::SeqCst);
                            // 失败条目**已经**计入 `failed`（outcome.failures 含这条
                            // 编码失败）—— 这里不再重复加一次。
                            logger.warn(
                                "REPORT_UNDECODABLE",
                                &[
                                    ("study_no", study_no.clone()),
                                    ("file", candidate.path.display().to_string()),
                                    ("failures", outcome.failures.to_string()),
                                ],
                            );
                        } else {
                            logger.info(
                                "REPORT_PROCESSED",
                                &[
                                    ("study_no", study_no.clone()),
                                    ("items", outcome.items.to_string()),
                                    ("failures", outcome.failures.to_string()),
                                ],
                            );
                        }
                    }
                    Err(e) => {
                        metrics.errors.fetch_add(1, Ordering::SeqCst);
                        logger.error(
                            "REPORT_FAILED",
                            &[
                                ("study_no", study_no.clone()),
                                ("error", e.to_string()),
                            ],
                        );
                        // 处理失败 → 把候选交还事件源，下一轮重新取件
                        // （源文件没被改动，重新处理是安全的）。
                        {
                            let mut watcher = watcher.lock().unwrap_or_else(|e| e.into_inner());
                            watcher.release(&candidate.path);
                        }
                    }
                }

                // 释放在途标记：同一检查号可以再被领走。
                {
                    let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.in_flight.remove(&study_no);
                }
                queue.cv.notify_all();
            }
        })
    }
}

/// 取一个可执行的任务（并标记在途）。
///
/// **同一检查号不并发**：在途集合里有它的检查号时，推迟到 `deferred`，
/// 等前面的处理完再领。库级兜底是幂等键唯一索引（不依赖这份内存态）。
fn take_task(state: &mut QueueState) -> Option<Candidate> {
    loop {
        // 被推迟的先试（它们只是因为同检查号在途才挪到这里的）。
        if let Some(index) = state
            .deferred
            .iter()
            .position(|candidate| !state.in_flight.contains(&candidate.study_no))
        {
            let candidate = state.deferred.remove(index).expect("位置有效");
            state.in_flight.insert(candidate.study_no.clone());
            return Some(candidate);
        }
        match state.ready.pop_front() {
            Some(candidate) => {
                if state.in_flight.contains(&candidate.study_no) {
                    state.deferred.push_back(candidate);
                    continue;
                }
                state.in_flight.insert(candidate.study_no.clone());
                return Some(candidate);
            }
            None => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::processor::{IngestProcessor, ProcessError, ProcessOutcome};
    use crate::ingest::watcher::CandidateKind;
    use std::path::Path;
    use crate::state::repo::Repo;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// 慢处理器：先睡一会儿再解析落库 —— 让并发峰值**可观测**。
    struct SlowProcessor {
        delay: Duration,
    }

    impl ReportProcessor for SlowProcessor {
        fn process(
            &self,
            repo: &Repo,
            candidate: &Candidate,
        ) -> Result<ProcessOutcome, ProcessError> {
            std::thread::sleep(self.delay);
            IngestProcessor.process(repo, candidate)
        }
    }

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
        let dir = std::env::temp_dir().join(format!("urs-sched-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 投递一份报告（模拟超声工作站导出）。
    fn deliver(inbox: &Path, study_no: &str, body: &str) {
        let dir = inbox.join(study_no);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.txt"), body).unwrap();
    }

    fn scheduler_config(inbox: &Path, db: &Path, max_concurrency: usize) -> DaemonConfig {
        DaemonConfig {
            inbox: inbox.to_path_buf(),
            state_db: db.to_path_buf(),
            max_concurrency,
            tick_interval: Duration::from_millis(50),
            min_stable: Duration::from_millis(50),
            heartbeat_every_ticks: 1,
        }
    }

    fn count(db: &Path, table: &str) -> i64 {
        let repo = Repo::open(db).unwrap();
        repo.connection()
            .query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    /// 等到条件成立或超时。
    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    fn candidate_of(study_no: &str, body: &str) -> Candidate {
        Candidate {
            study_no: study_no.to_string(),
            path: PathBuf::from(format!("inbox/{}/report.txt", study_no)),
            kind: CandidateKind::Txt,
            byte_len: body.len() as u64,
            idempotency_key: crate::state::idempotency_key(study_no, body),
            raw_bytes: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn 并发峰值不超过上限且八份报告全部处理() {
        let dir = temp_dir("concurrency");
        let inbox = dir.join("inbox");
        let db = dir.join("state.sqlite");
        std::fs::create_dir_all(&inbox).unwrap();
        for i in 0..8 {
            deliver(
                &inbox,
                &format!("S-CONC-{:02}", i),
                "病历号：PID-EXAMPLE\n左肾积水 3.2cm\n",
            );
        }

        let buffer = Buffer::default();
        let logger = Arc::new(Logger::with_sink(Box::new(buffer.clone())));
        let scheduler = Scheduler::new(
            scheduler_config(&inbox, &db, 4),
            Arc::new(SlowProcessor {
                delay: Duration::from_millis(40),
            }),
            logger,
        );
        let metrics = scheduler.metrics();
        let shutdown = scheduler.shutdown_flag();
        let handle = std::thread::spawn(move || scheduler.run());

        assert!(
            wait_until(|| metrics.processed.load(Ordering::SeqCst) >= 8, Duration::from_secs(20)),
            "8 份报告都应被处理，实际 processed={}",
            metrics.processed.load(Ordering::SeqCst)
        );
        let peak = metrics.peak_concurrency.load(Ordering::SeqCst);
        assert!(peak <= 4, "并发峰值不得超过上限 4，实际 {}", peak);
        assert!(peak >= 2, "并发峰值应大于 1（确实并行处理了），实际 {}", peak);
        assert_eq!(metrics.processed.load(Ordering::SeqCst), 8);
        assert_eq!(count(&db, "items"), 8, "每份报告一条台账行");

        shutdown.store(true, Ordering::SeqCst);
        handle.join().unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 优雅退出后在途任务跑完并落库() {
        let dir = temp_dir("graceful");
        let inbox = dir.join("inbox");
        let db = dir.join("state.sqlite");
        std::fs::create_dir_all(&inbox).unwrap();
        deliver(&inbox, "S-GRACE", "病历号：PID-EXAMPLE\n左肾积水 3.2cm\n");

        let buffer = Buffer::default();
        let logger = Arc::new(Logger::with_sink(Box::new(buffer.clone())));
        let scheduler = Scheduler::new(
            scheduler_config(&inbox, &db, 2),
            Arc::new(SlowProcessor {
                delay: Duration::from_millis(300),
            }),
            logger,
        );
        let metrics = scheduler.metrics();
        let shutdown = scheduler.shutdown_flag();
        let handle = std::thread::spawn(move || scheduler.run());

        // 等到任务真的在处理（进入在途）。
        assert!(
            wait_until(|| metrics.in_flight.load(Ordering::SeqCst) > 0, Duration::from_secs(20)),
            "应观察到在途任务"
        );
        // 停机：停止取新任务，等在途任务结束。
        shutdown.store(true, Ordering::SeqCst);
        handle.join().unwrap().unwrap();

        assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 0, "退出后不应还有在途任务");
        assert_eq!(
            count(&db, "items"),
            1,
            "在途任务应跑完并落库（不是被丢弃）"
        );
        let text = buffer.text();
        assert!(text.contains("DAEMON_STOPPING"), "{}", text);
        assert!(text.contains("DAEMON_STOPPED"), "{}", text);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 停机后不再领取新任务() {
        let dir = temp_dir("stop");
        let inbox = dir.join("inbox");
        let db = dir.join("state.sqlite");
        std::fs::create_dir_all(&inbox).unwrap();
        for i in 0..4 {
            deliver(&inbox, &format!("S-STOP-{:02}", i), "左肾积水 3.2cm\n");
        }

        let buffer = Buffer::default();
        let logger = Arc::new(Logger::with_sink(Box::new(buffer.clone())));
        let scheduler = Scheduler::new(
            scheduler_config(&inbox, &db, 1),
            Arc::new(IngestProcessor),
            logger,
        );
        let metrics = scheduler.metrics();
        let shutdown = scheduler.shutdown_flag();
        let handle = std::thread::spawn(move || scheduler.run());

        // 停机：调度线程退出，不再领取新任务。
        shutdown.store(true, Ordering::SeqCst);
        handle.join().unwrap().unwrap();
        let processed_after_stop = metrics.processed.load(Ordering::SeqCst);

        // 停机后再投递：不应被领取。
        deliver(&inbox, "S-STOP-LATE", "左肾积水 3.2cm\n");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            metrics.processed.load(Ordering::SeqCst),
            processed_after_stop,
            "停机后不得再处理新投递"
        );
        assert_eq!(count(&db, "items"), processed_after_stop as i64);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 同一检查号不并发处理() {
        // 派发层规则：同一 `study_no` 在途时不并发领第二份；不同检查号照常并行。
        let mut state = QueueState::default();
        state
            .ready
            .push_back(candidate_of("S-SAME", "左肾积水 3.2cm\n"));
        state
            .ready
            .push_back(candidate_of("S-SAME", "左肾积水 4.1cm\n"));
        state
            .ready
            .push_back(candidate_of("S-OTHER", "左肾积水 5.0cm\n"));

        let first = take_task(&mut state).expect("应领到第一份");
        assert_eq!(first.study_no, "S-SAME");
        assert!(state.in_flight.contains("S-SAME"));

        // 第二份同检查号：被推迟，不得并发领走；不同检查号照常领走。
        let second = take_task(&mut state).expect("不同检查号应可并行");
        assert_eq!(second.study_no, "S-OTHER");
        assert_eq!(state.deferred.len(), 1, "同检查号的第二份应进 deferred 队列");
        assert_eq!(state.deferred[0].study_no, "S-SAME");
        assert_eq!(
            state.in_flight.len(),
            2,
            "两份不同检查号可同时在途，同检查号只有一份在途"
        );

        // 前一份处理完（释放在途标记）后，被推迟的那份才可领走。
        state.in_flight.remove("S-SAME");
        let deferred = take_task(&mut state).expect("在途释放后应能领走推迟的任务");
        assert_eq!(deferred.study_no, "S-SAME");
        assert_eq!(state.deferred.len(), 0);
    }
}
