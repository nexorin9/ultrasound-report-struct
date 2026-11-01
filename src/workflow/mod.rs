//! 工作流：状态机、审计、幂等、并发保护、运行摘要。
//!
//! | 子模块 | 落点 | 状态 |
//! |---|---|---|
//! | [`state`] | 报告状态机与非法迁移守卫（**产物语义 = 待确认草稿**的代码化） | 已落地 |
//! | [`audit`] | 逐动作留痕（九类动作） | 已落地 |
//! | [`idempotency`] | 幂等键与事务边界（重复投递不产生第二条台账行） | 已落地 |
//! | [`concurrency`] | 库级检查号锁（同检查号串行、不同检查号并行） | 已落地 |
//! | [`summary`] | 运行摘要（库口径：处理数 / 失败率 / 待确认数 / 按错误码聚合） | 已落地 |
//!
//! **边界**：本层只管**状态怎么走**与**动作怎么留痕**，不管**状态怎么落库**。
//! 落库推进的调用点在确认回执端点（Task 40）与回写退避重试（Task 41）。

pub mod audit;
pub mod concurrency;
pub mod idempotency;
pub mod state;
pub mod summary;

pub use audit::{
    compared_detail, coercion_detail, input_digest, Actor, AuditAction, AuditContext,
    AuditRecorder, AuditRow, ACTION_COMPARED, ACTION_COERCED, ACTION_CONFIRMED,
    ACTION_DUPLICATE_SKIPPED, ACTION_FAILED, ACTION_PARSED, ACTION_PUSHED, ACTION_RECEIVED,
    ACTION_REJECTED, ACTION_WRITTEN_BACK,
};
pub use concurrency::{
    ConcurrencyError, StudyNoGate, StudyNoLease, DEFAULT_ATTEMPTS, DEFAULT_BACKOFF_MS,
    DEFAULT_LEASE_SECS,
};
pub use idempotency::{
    idempotency_key, idempotency_key_text, record_duplicate_skipped, IdempotencyGuard,
    IdempotencyOutcome, SCOPE_INGEST, SCOPE_WRITEBACK,
};
pub use state::{
    allowed_targets, can_transition, guard, transition, ReviewDecision, Status, TransitionError,
    TransitionErrorKind, TRANSITION_TABLE,
};
pub use summary::{
    summarize, DailySummary, SummaryError, SummaryWindow,
};
