//! 守护调度：把「事件源」接成**常驻进程**。
//!
//! 本轮交付的形态是**事件驱动守护**（`spec.md` §7）：超声工作站把报告投进
//! `inbox/<检查号>/report.txt`，守护进程常驻监听，**事件到达即处理**，
//! 不退回「跑一次出一次表」的批处理脚本。
//!
//! 分工：
//!
//! | 层 | 位置 | 职责 |
//! |---|---|---|
//! | 事件源 | [`crate::ingest::watcher`] | 目录轮询 + 写入稳定判定 + 幂等预查（Task 8） |
//! | **调度** | [`scheduler`] | 任务队列 + 并发上限 + 优雅退出 + 心跳（**本模块**） |
//! | 处理 | [`processor`] | 候选 → 解析 → 强转 → 落库（默认处理器与 `ingest` 子命令同一条路径） |
//! | 停机 | [`signal`] | SIGTERM / SIGINT → 置停止标志（**不新增依赖**） |
//!
//! **并发的两条边界**：
//! 1. **不同检查号并行** —— 并发上限（默认 4）由调度器控制；
//! 2. **同一检查号串行** —— 调度器在**派发层**不并发派发同一 `study_no`，
//!    但**正确性不依赖这份内存态**：幂等键（`sha256(study_no‖报告内容)`）的唯一索引
//!    在**数据库层**兜底 —— 进程重启、并发重投都不会产生第二条台账行。
//!    库级串行化（跨进程粒度）在 Task 36 单独落地。

pub mod processor;
pub mod scheduler;
pub mod signal;

pub use processor::{
    ComparingProcessor, IngestProcessor, ProcessError, ProcessOutcome, ReportProcessor,
};
pub use scheduler::{DaemonConfig, Scheduler, SchedulerError, SchedulerMetrics};
