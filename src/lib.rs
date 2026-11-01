//! 超声报告结构化系统 —— 库入口。
//!
//! 模块分层（与产品主路径一一对应）：
//! - `parse` —— 解析内核：词法扫描 → 前瞻合并 → 抽取 → 失败定位。
//! - `coerce` —— 强转内核：量纲路径判定 → 院内口径数值。
//! - `ingest` —— 入站适配：投递目录 → 报告正文 → 事件源（监听 + 去抖）。
//! - `daemon` —— 守护调度：任务队列 + 并发上限 + 优雅退出 + 心跳（常驻形态的落点）。
//! - `state` —— 状态库：schema、迁移、仓储（事务边界）、幂等键。
//! - `workflow` —— 工作流：状态机（非法迁移守卫）、审计、幂等、并发保护。
//! - `outbound` —— 出站：EMR 结构化报告回写（真实出站写）、企微推送、确认回执端点。
//! - `export` —— 台账 / 失败清单 CSV 导出。
//! - `config` —— 配置（路径与端点）+ 凭据（环境变量）+ 启动自检。
//! - `log` —— 结构化行日志 + 运行摘要（心跳）。
//! - `cli` —— 入口层：全局参数 + 运维通道（离线重放 / 自检）。

pub mod cli;
pub mod coerce;
pub mod compare;
pub mod config;
pub mod daemon;
pub mod export;
pub mod ingest;
pub mod log;
pub mod outbound;
pub mod parse;
pub mod publicdata;
pub mod state;
pub mod workflow;

/// 部位 / 测项词典版本（由词典自身版本号给出，不再写死在代码里）。
///
/// 口径追溯用：每条台账行都落这个版本号，口径变了历史行仍留旧版本。
/// 部位版本来自**已加载的部位词典文件**（`assets/dictionary/parts.json`），
/// 词典改了版本号也改，台账行跟着改 —— 写死的版本号会骗人。
pub fn dictionary_version() -> String {
    coerce::dictionary::dictionary_version()
}
