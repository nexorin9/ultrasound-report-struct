//! 工作流第 5 层：运行摘要与可观测性。
//!
//! **这一层解决的接缝**：守护进程在超声科机房里跑了一夜，第二天早上运维接班，
//! 第一句问的是「**昨天有多少报告没解析出来**」—— 现在没人答得上来：
//! 报告确实在跑，但数字不在手边；出了事也只能一份份翻失败清单。
//!
//! 本层把这个问题变成**一条命令 / 一行日志**。
//!
//! ## 两个口径，别混
//!
//! | 口径 | 在哪 | 活多久 | 回答什么 |
//! |---|---|---|---|
//! | **进程内计数** | [`crate::log::RunSummary`] | 进程活着的这段时间 | 本次运行处理了多少份 |
//! | **库口径摘要**（本层） | [`DailySummary`] | **跨重启、跨进程** | 某一天受理了多少份、哪些没解析出来 |
//!
//! 守护进程重启后，进程内计数归零，**库口径还在** —— 运维问的是「昨天」，
//! 昨天的事不该因为今早重启过一次就查不到了。所以摘要一律**从状态库算**，不读内存。
//!
//! ## 窗口与存量是两回事
//!
//! - **窗口内**（当天 / 指定某日）：`processed` / `failed` / `failed_items` /
//!   `duplicates` / `by_error_code` —— 这一天**新增**了多少、坏了多少。
//! - **当前存量**（截至此刻）：`awaiting_review` / `pending_writeback` / `by_status` ——
//!   现在还压着多少活。存量**不带窗口**：一份前天到的报告今天还在等医师确认，
//!   它就还占着一个「待确认」的名额（昨天、前天的窗口都不该把它算进去）。
//!
//! ## 失败率的口径
//!
//! `failed_rate` = **整份没解析出来的报告份数** ÷ 受理份数。
//!
//! 「整份」是关键词：一份报告里 3 条测值坏了 1 条，那份报告**仍然出得来台账行**
//! （好条目照常落库），它算「部分失败」，进 `failed_items`（失败条目数），
//! **不算**「没解析出来」。两个数都给出，运维自己看是「坏了一部分」还是「整份没出」。
//!
//! ## 失败按错误码聚合 = 定位「谁该去修」
//!
//! [`by_error_code`] 按稳定错误码聚合计数，[`by_disposition`] 再按
//! **运维处置动作**聚合（待补词典 / 待修写法 / 整份退回 / 待上游恢复 / 待人工判定）。
//!
//! 这两层的意义：值班的人看一眼就知道该找谁 ——
//! 编码问题（`ENCODING_MIXED`）找工作站侧，词典缺口（`ITEM_DICT_MISSING`）找信息科补词表，
//! 上游不可达（`RIS_UPSTREAM_UNAVAILABLE`）查网络与令牌。三类都记成「失败」，
//! 等于让人自己去翻失败清单逐条猜。

use std::collections::BTreeMap;
use std::fmt;

use rusqlite::params;

use crate::parse::errors::ErrorCode;
use crate::state::repo::{Repo, RepoError};
use crate::workflow::audit::{ACTION_DUPLICATE_SKIPPED, ACTION_FAILED};
use crate::workflow::state::Status;

/// 窗口无界时的**兜底边界**（让所有查询保持同一套 SQL 形状）。
///
/// 时间戳是 `YYYY-MM-DD HH:mm:ss` 定长字符串，**字典序 = 时间序**，
/// 所以用哨兵值代替「无下界 / 无上界」是安全的。
const MIN_TIMESTAMP: &str = "0000-01-01 00:00:00";
const MAX_TIMESTAMP: &str = "9999-12-31 23:59:59";

/// **摘要的时间窗口**。
///
/// 窗口是**左闭右开**的 `[start, end)` —— 跨日的边界不会把一份报告算进两天。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryWindow {
    /// 起始时刻（含）；`None` = 不限。
    start: Option<String>,
    /// 结束时刻（不含）；`None` = 不限。
    end: Option<String>,
    /// 窗口标签（给人看的：`2026-11-01` 或 `全部`）。
    label: String,
}

impl SummaryWindow {
    /// **不限窗口**（库里全部）。
    pub fn all() -> SummaryWindow {
        SummaryWindow {
            start: None,
            end: None,
            label: "全部".to_string(),
        }
    }

    /// **某个自然日**（UTC，`[当天 00:00:00, 次日 00:00:00)`）。
    ///
    /// 日期格式 `YYYY-MM-DD`；格式不对返回 `None`（**不猜**是哪一天）。
    pub fn day(date: &str) -> Option<SummaryWindow> {
        let (y, m, d) = parse_civil_date(date)?;
        let start = format!("{:04}-{:02}-{:02} 00:00:00", y, m, d);
        let next = crate::state::days_from_civil(y, m, d) + 1;
        let (ny, nm, nd) = crate::state::civil_from_days(next);
        let end = format!("{:04}-{:02}-{:02} 00:00:00", ny, nm, nd);
        Some(SummaryWindow {
            start: Some(start),
            end: Some(end),
            label: format!("{:04}-{:02}-{:02}", y, m, d),
        })
    }

    /// **今天**（UTC）。
    pub fn today() -> SummaryWindow {
        let date = current_date();
        SummaryWindow::day(&date).expect("当前日期必然合法")
    }

    /// **昨天**（UTC）—— 运维接班时问的通常就是这一天。
    pub fn yesterday() -> SummaryWindow {
        let (y, m, d) = parse_civil_date(&current_date()).expect("当前日期必然合法");
        let prev = crate::state::days_from_civil(y, m, d) - 1;
        let (py, pm, pd) = crate::state::civil_from_days(prev);
        SummaryWindow::day(&format!("{:04}-{:02}-{:02}", py, pm, pd))
            .expect("换算出来的日期必然合法")
    }

    /// 起始时刻（含）。
    pub fn start(&self) -> Option<&str> {
        self.start.as_deref()
    }

    /// 结束时刻（不含）。
    pub fn end(&self) -> Option<&str> {
        self.end.as_deref()
    }

    /// 窗口标签。
    pub fn label(&self) -> &str {
        &self.label
    }

    /// 查询用的**实际下界**（无界时用哨兵值）。
    fn bound_start(&self) -> &str {
        self.start.as_deref().unwrap_or(MIN_TIMESTAMP)
    }

    /// 查询用的**实际上界**（无界时用哨兵值）。
    fn bound_end(&self) -> &str {
        self.end.as_deref().unwrap_or(MAX_TIMESTAMP)
    }
}

/// 当前 UTC 日期（`YYYY-MM-DD`）。
fn current_date() -> String {
    crate::state::now_datetime()[..10].to_string()
}

/// 解析 `YYYY-MM-DD`（格式不对返回 `None` —— **不猜**是哪一天）。
fn parse_civil_date(date: &str) -> Option<(i64, u32, u32)> {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = date[0..4].parse().ok()?;
    let month: u32 = date[5..7].parse().ok()?;
    let day: u32 = date[8..10].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// **每日运行摘要**（库口径）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DailySummary {
    /// 窗口标签。
    pub window_label: String,
    /// 窗口起始（含）。
    pub window_start: Option<String>,
    /// 窗口结束（不含）。
    pub window_end: Option<String>,
    /// 窗口内**受理**的报告份数。
    pub processed: u64,
    /// 窗口内**整份没解析出来**的报告份数。
    pub failed: u64,
    /// 窗口内失败**条目**数（含部分失败）。
    pub failed_items: u64,
    /// 整份失败率（`failed / processed`）。
    pub failed_rate: f64,
    /// 窗口内被幂等拦下的重复投递份数。
    pub duplicates: u64,
    /// **当前**还没走完「医师确认」这一关的报告份数（存量）。
    pub awaiting_review: u64,
    /// **当前**待回写重试的报告份数（存量）。
    pub pending_writeback: u64,
    /// **当前**各状态的报告份数（存量）。
    pub by_status: BTreeMap<String, u64>,
    /// 窗口内失败条目按稳定错误码聚合。
    pub by_error_code: BTreeMap<String, u64>,
    /// 窗口内失败条目按运维处置动作聚合。
    pub by_disposition: BTreeMap<String, u64>,
    /// 最近一条审计事件的时间。
    pub last_event_at: Option<String>,
}

impl DailySummary {
    /// 整份失败率（受理数为 0 时取 0 —— **不除零**）。
    pub fn failed_rate(&self) -> f64 {
        if self.processed == 0 {
            return 0.0;
        }
        self.failed as f64 / self.processed as f64
    }

    /// 失败条目按错误码排序（**条数多的在前**，同数按码名字典序）。
    pub fn sorted_error_codes(&self) -> Vec<(&str, u64)> {
        let mut rows: Vec<(&str, u64)> = self
            .by_error_code
            .iter()
            .map(|(code, count)| (code.as_str(), *count))
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        rows
    }

    /// 库里出现、但**不在稳定码表**里的错误码。
    ///
    /// 正常情况下**永远是空的** —— 全部失败都收敛到
    /// [`crate::parse::errors::ErrorCode`] 那张表（`spec.md` §4「失败分类口径」）。
    /// 这里给出它，是为了在有人绕过码表塞了个自由文本进来时**当场看得见**，
    /// 而不是让它混进按码聚合里悄悄消失。
    pub fn unknown_error_codes(&self) -> Vec<&str> {
        self.by_error_code
            .keys()
            .filter(|code| ErrorCode::lookup(code).is_none())
            .map(|code| code.as_str())
            .collect()
    }

    /// 某个错误码的中文说明（**未知码不猜**，原样返回码本身）。
    pub fn error_label(&self, code: &str) -> String {
        match ErrorCode::lookup(code) {
            Some(known) => known.message().to_string(),
            None => format!("{}（未登记的错误码）", code),
        }
    }

    /// 错误码的运维处置动作（**未知码不猜**，返回 `None`）。
    pub fn error_disposition(&self, code: &str) -> Option<&'static str> {
        ErrorCode::lookup(code).map(|known| known.disposition().as_str())
    }

    /// 渲染成一行（心跳 / 日志用；字段之间用空格，便于 grep）。
    pub fn render(&self) -> String {
        format!(
            "运行摘要 window={} processed={} failed={} failed_rate={:.3} failed_items={} \
             duplicates={} awaiting_review={} pending_writeback={} error_codes={}",
            self.window_label,
            self.processed,
            self.failed,
            self.failed_rate(),
            self.failed_items,
            self.duplicates,
            self.awaiting_review,
            self.pending_writeback,
            self.render_error_codes()
        )
    }

    /// 错误码分布的一行表示（`UNIT_MISSING:3,ITEM_DICT_MISSING:1`；无失败时为 `-`）。
    pub fn render_error_codes(&self) -> String {
        if self.by_error_code.is_empty() {
            return "-".to_string();
        }
        self.sorted_error_codes()
            .into_iter()
            .map(|(code, count)| format!("{}:{}", code, count))
            .collect::<Vec<String>>()
            .join(",")
    }

    /// 渲染成运维简报（多行；回答「昨天有多少报告没解析出来」）。
    pub fn render_report(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("运行摘要（窗口 {}，UTC）\n", self.window_label));
        out.push_str(&format!("  受理报告：{} 份\n", self.processed));
        out.push_str(&format!(
            "  整份没解析出来：{} 份（整份失败率 {}）\n",
            self.failed,
            percent(self.failed_rate())
        ));
        out.push_str(&format!("  失败条目：{} 条\n", self.failed_items));
        out.push_str(&format!("  重复投递跳过：{} 份\n", self.duplicates));
        out.push_str(&format!(
            "  待医师确认：{} 份（当前存量）\n",
            self.awaiting_review
        ));
        out.push_str(&format!(
            "  待回写重试：{} 份（当前存量）\n",
            self.pending_writeback
        ));

        out.push_str("  失败原因（按错误码 / 处置动作）：\n");
        if self.by_error_code.is_empty() {
            out.push_str("    （窗口内没有失败条目）\n");
        } else {
            for (code, count) in self.sorted_error_codes() {
                out.push_str(&format!("    {} {} 条\n", code, count));
                match self.error_disposition(code) {
                    Some(d) => out.push_str(&format!(
                        "      处置：{}（{}）\n",
                        disposition_message(d).unwrap_or(d),
                        d
                    )),
                    None => out.push_str("      处置：未登记（不猜处置动作）\n"),
                }
                out.push_str(&format!("      说明：{}\n", self.error_label(code)));
            }
        }

        out.push_str("  当前状态分布：\n");
        if self.by_status.is_empty() {
            out.push_str("    （库里还没有报告）\n");
        } else {
            for (status, count) in &self.by_status {
                out.push_str(&format!("    {} {} 份\n", status, count));
            }
        }

        out.push_str(&format!(
            "  最近事件时间：{}\n",
            self.last_event_at.as_deref().unwrap_or("-")
        ));
        out
    }
}

/// 失败率渲染成百分比（一位小数）。
fn percent(rate: f64) -> String {
    format!("{:.1}%", rate * 100.0)
}

/// 处置动作的中文说明（**反查码表，不另抄一份说法**）。
///
/// 处置动作的文案只有一处定义（[`crate::parse::errors::Disposition::message`]），
/// 这里按处置码反查回来 —— 同一件事写两遍说法，迟早漂移。
fn disposition_message(disposition_code: &str) -> Option<&'static str> {
    ErrorCode::ALL
        .iter()
        .map(|code| code.disposition())
        .find(|d| d.as_str() == disposition_code)
        .map(|d| d.message())
}

/// **摘要查询失败**。
#[derive(Debug)]
pub enum SummaryError {
    /// 状态库错误。
    Repo(RepoError),
    /// 日期格式不对（`YYYY-MM-DD`）。
    InvalidDate(String),
}

impl fmt::Display for SummaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SummaryError::Repo(e) => write!(f, "状态库错误：{}", e),
            SummaryError::InvalidDate(raw) => {
                write!(f, "日期格式须为 YYYY-MM-DD：{}", raw)
            }
        }
    }
}

impl std::error::Error for SummaryError {}

impl From<RepoError> for SummaryError {
    fn from(e: RepoError) -> Self {
        SummaryError::Repo(e)
    }
}

/// **算一份运行摘要**（从状态库读，不读内存）。
///
/// 全部数字都来自**权威存储**：报告状态取 `reports.status`，
/// 失败条目取 `audit_log` 里 `failed` 动作的留痕（带稳定错误码）。
pub fn summarize(repo: &Repo, window: &SummaryWindow) -> Result<DailySummary, SummaryError> {
    let start = window.bound_start();
    let end = window.bound_end();

    // 1) 窗口内受理的报告份数。
    let processed = count2(
        repo,
        "SELECT COUNT(*) FROM reports WHERE created_at >= ?1 AND created_at < ?2",
        start,
        end,
    )?;

    // 2) 窗口内**整份**没解析出来的报告份数（状态为 parse_failed）。
    let failed = count3(
        repo,
        "SELECT COUNT(*) FROM reports WHERE status = ?1 AND created_at >= ?2 AND created_at < ?3",
        Status::ParseFailed.as_str(),
        start,
        end,
    )?;

    // 3) 窗口内失败条目数（含部分失败 —— 报告出得来，但有测值坏了）。
    let failed_items = count3(
        repo,
        "SELECT COUNT(*) FROM audit_log WHERE action = ?1 AND created_at >= ?2 AND created_at < ?3",
        ACTION_FAILED,
        start,
        end,
    )?;

    // 4) 窗口内被幂等拦下的重复投递份数。
    let duplicates = count3(
        repo,
        "SELECT COUNT(*) FROM audit_log WHERE action = ?1 AND created_at >= ?2 AND created_at < ?3",
        ACTION_DUPLICATE_SKIPPED,
        start,
        end,
    )?;

    // 5) 当前各状态份数（**存量**，不带窗口）。
    let by_status = group_counts0(
        repo,
        "SELECT status, COUNT(*) FROM reports GROUP BY status ORDER BY status",
    )?;
    let awaiting_review = by_status
        .iter()
        .filter(|(status, _)| {
            // 这三种状态都**还没等来医师点头**：
            // received（正文读到了还没解析）、parsed（出条目了还没确认）、
            // awaiting_review（明摆着等人确认）。解析失败是终态，不占名额。
            status.as_str() == Status::Received.as_str()
                || status.as_str() == Status::Parsed.as_str()
                || status.as_str() == Status::AwaitingReview.as_str()
        })
        .map(|(_, count)| *count)
        .sum();
    let pending_writeback = by_status
        .get(Status::PendingWriteback.as_str())
        .copied()
        .unwrap_or(0);

    // 6) 窗口内失败条目按错误码聚合。
    let by_error_code = group_counts3(
        repo,
        "SELECT error_code, COUNT(*) FROM audit_log \
         WHERE action = ?1 AND error_code IS NOT NULL AND created_at >= ?2 AND created_at < ?3 \
         GROUP BY error_code",
        ACTION_FAILED,
        start,
        end,
    )?;

    // 7) 失败条目按**运维处置动作**再聚合一层（回答「该找谁」）。
    //    未知码不进这张表 —— 不给未登记的码硬套一个处置动作。
    let mut by_disposition: BTreeMap<String, u64> = BTreeMap::new();
    for (code, count) in &by_error_code {
        if let Some(known) = ErrorCode::lookup(code) {
            *by_disposition
                .entry(known.disposition().as_str().to_string())
                .or_insert(0) += *count;
        }
    }

    // 8) 最近一条审计事件的时间（摘要的「截止到什么时候」）。
    let last_event_at: Option<String> = repo
        .connection()
        .query_row("SELECT MAX(created_at) FROM audit_log", [], |row| {
            row.get::<_, Option<String>>(0)
        })
        .map_err(RepoError::from)?;

    Ok(DailySummary {
        window_label: window.label().to_string(),
        window_start: window.start().map(|s| s.to_string()),
        window_end: window.end().map(|s| s.to_string()),
        processed,
        failed,
        failed_items,
        failed_rate: 0.0,
        duplicates,
        awaiting_review,
        pending_writeback,
        by_status,
        by_error_code,
        by_disposition,
        last_event_at,
    })
}

/// 计数查询（单值，两参数）。
fn count2(repo: &Repo, sql: &str, a: &str, b: &str) -> Result<u64, RepoError> {
    let value: i64 = repo
        .connection()
        .query_row(sql, params![a, b], |row| row.get(0))?;
    Ok(value.max(0) as u64)
}

/// 计数查询（单值，三参数）。
fn count3(repo: &Repo, sql: &str, a: &str, b: &str, c: &str) -> Result<u64, RepoError> {
    let value: i64 = repo
        .connection()
        .query_row(sql, params![a, b, c], |row| row.get(0))?;
    Ok(value.max(0) as u64)
}

/// 分组计数（键 → 条数，无参数）。
fn group_counts0(repo: &Repo, sql: &str) -> Result<BTreeMap<String, u64>, RepoError> {
    let mut stmt = repo.connection().prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (key, count) = row?;
        out.insert(key, count.max(0) as u64);
    }
    Ok(out)
}

/// 分组计数（键 → 条数，三参数）。
fn group_counts3(
    repo: &Repo,
    sql: &str,
    a: &str,
    b: &str,
    c: &str,
) -> Result<BTreeMap<String, u64>, RepoError> {
    let mut stmt = repo.connection().prepare(sql)?;
    let rows = stmt.query_map(params![a, b, c], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (key, count) = row?;
        out.insert(key, count.max(0) as u64);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::processor::{IngestProcessor, ReportProcessor};
    use crate::ingest::watcher::{Candidate, CandidateKind};
    use crate::state::idempotency_key;
    use std::path::PathBuf;

    /// 造一个投递候选（守护进程拿到的那种候选）。
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

    /// 一份能解析出条目的报告。
    const GOOD: &str = "超声所见：\n左肾前后径 3.2cm";

    /// 注入 n 份好报告与 m 份坏报告（走**真实入库路径**：解析 → 强转 → 落库 → 留痕）。
    ///
    /// 每份坏报告**只有一条**失败条目（缺单位）—— 坏件数与失败条目数对得上，
    /// 口径才不会被样本自身搅浑。
    fn inject(repo: &Repo, good: usize, bad: usize) {
        for i in 0..good {
            let body = format!("{}\n膀胱残余尿量 约 {}ml", GOOD, 100 + i);
            IngestProcessor
                .process(repo, &candidate(&format!("S-GOOD-{:03}", i), &body))
                .expect("好报告应处理成功");
        }
        for i in 0..bad {
            let body = format!("超声所见：\n右肾积水 {}.1", i + 1);
            IngestProcessor
                .process(repo, &candidate(&format!("S-BAD-{:03}", i), &body))
                .expect("坏报告应处理成功（落失败条目）");
        }
    }

    #[test]
    fn 十份样例含三份坏件计数正确() {
        let repo = Repo::open_in_memory().unwrap();
        inject(&repo, 7, 3);

        let summary = summarize(&repo, &SummaryWindow::today()).unwrap();
        assert_eq!(summary.processed, 10, "受理 10 份");
        assert_eq!(summary.failed, 3, "3 份整份没解析出来");
        assert_eq!(summary.failed_items, 3, "每份坏件一条失败条目");
        assert!((summary.failed_rate() - 0.3).abs() < 1e-9, "失败率 0.3");
        assert_eq!(summary.by_status.get("parsed").copied(), Some(7));
        assert_eq!(summary.by_status.get("parse_failed").copied(), Some(3));
        assert_eq!(
            summary.by_error_code.get("UNIT_MISSING").copied(),
            Some(3),
            "坏件都是缺单位"
        );
        // 未知码表里不该有码（失败全部收敛到稳定码表）。
        assert!(
            summary.unknown_error_codes().is_empty(),
            "出现了未登记的错误码：{:?}",
            summary.unknown_error_codes()
        );
        // 处置动作：缺单位 → 待修写法。
        assert_eq!(
            summary.by_disposition.get("AWAITING_VALUE").copied(),
            Some(3)
        );
    }

    #[test]
    fn 部分失败不算整份失败() {
        let repo = Repo::open_in_memory().unwrap();
        // 一份报告：左肾能解析、右肾缺单位 —— 台账行照常出得来。
        let body = "超声所见：\n左肾前后径 3.2cm\n右肾积水 4.1";
        let out = IngestProcessor
            .process(&repo, &candidate("S-PART-001", body))
            .unwrap();
        assert_eq!(out.items, 1, "好条目照常产出");
        assert_eq!(out.failures, 1);

        let summary = summarize(&repo, &SummaryWindow::today()).unwrap();
        assert_eq!(summary.processed, 1);
        assert_eq!(summary.failed, 0, "部分失败不是整份失败");
        assert_eq!(summary.failed_items, 1, "但失败条目要计一条");
        assert_eq!(summary.by_status.get("parsed").copied(), Some(1));
    }

    #[test]
    fn 待确认与待回写按当前存量计数() {
        let repo = Repo::open_in_memory().unwrap();
        inject(&repo, 4, 1);
        // 人工推进状态：2 份待确认、1 份待回写。
        repo.connection()
            .execute(
                "UPDATE reports SET status = 'awaiting_review' WHERE study_no IN \
                 ('S-GOOD-000','S-GOOD-001')",
                [],
            )
            .unwrap();
        repo.connection()
            .execute(
                "UPDATE reports SET status = 'pending_writeback' WHERE study_no = 'S-GOOD-002'",
                [],
            )
            .unwrap();

        let summary = summarize(&repo, &SummaryWindow::today()).unwrap();
        // 没走完「医师确认」这一关的：received + parsed + awaiting_review
        // = 1（还停在 parsed）+ 2（已进 awaiting_review）= 3；
        // 那 1 份待回写的已经过了确认这一关，不计入；解析失败那 1 份是终态，也不计。
        assert_eq!(summary.awaiting_review, 3, "3 份还没等来医师点头");
        assert_eq!(summary.pending_writeback, 1);
        assert_eq!(
            summary.by_status.get("awaiting_review").copied(),
            Some(2),
            "状态分布要能看出细分"
        );
    }

    #[test]
    fn 窗口按日期切片昨天的报告不计入今天() {
        let repo = Repo::open_in_memory().unwrap();
        inject(&repo, 2, 1);
        // 把这批记录的时间戳挪到昨天（模拟「昨天跑的，今天才查」）。
        repo.connection()
            .execute(
                "UPDATE reports SET created_at = '2026-01-01 08:00:00'",
                [],
            )
            .unwrap();
        repo.connection()
            .execute(
                "UPDATE audit_log SET created_at = '2026-01-01 08:00:00.000'",
                [],
            )
            .unwrap();

        let today = summarize(&repo, &SummaryWindow::today()).unwrap();
        assert_eq!(today.processed, 0, "今天的窗口不该算上昨天的");
        assert_eq!(today.failed, 0);

        let yesterday = summarize(
            &repo,
            &SummaryWindow::day("2026-01-01").expect("日期合法"),
        )
        .unwrap();
        assert_eq!(yesterday.processed, 3, "昨天的窗口算昨天的");
        assert_eq!(yesterday.failed, 1);
        assert_eq!(yesterday.window_label, "2026-01-01");
    }

    #[test]
    fn 重复投递计入重复数但不产生第二条台账() {
        let repo = Repo::open_in_memory().unwrap();
        let body = GOOD.to_string();
        IngestProcessor
            .process(&repo, &candidate("S-DUP-001", &body))
            .unwrap();
        let again = IngestProcessor
            .process(&repo, &candidate("S-DUP-001", &body))
            .unwrap();
        assert!(again.duplicate, "重复投递应被判为幂等命中");

        let summary = summarize(&repo, &SummaryWindow::today()).unwrap();
        assert_eq!(summary.processed, 1, "重复投递不增加受理份数");
        assert_eq!(summary.duplicates, 1, "但要记一笔重复跳过");
    }

    #[test]
    fn 空库摘要全零且不除零() {
        let repo = Repo::open_in_memory().unwrap();
        let summary = summarize(&repo, &SummaryWindow::today()).unwrap();
        assert_eq!(summary.processed, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.failed_rate(), 0.0);
        assert!(summary.by_status.is_empty());
        assert_eq!(summary.render_error_codes(), "-");
        let report = summary.render_report();
        assert!(report.contains("受理报告：0 份"), "{}", report);
    }

    #[test]
    fn 未知错误码不猜处置动作且能被看见() {
        let repo = Repo::open_in_memory().unwrap();
        inject(&repo, 1, 0);
        // 绕过码表塞一条自由文本错误码（模拟有人手工改库 / 将来新增码忘了登记）。
        repo.connection()
            .execute(
                "INSERT INTO audit_log (study_no, action, actor, error_code, created_at) \
                 VALUES ('S-GOOD-000','failed','system','SOME_UNDECLARED_CODE', ?1)",
                params![crate::state::now_datetime_millis()],
            )
            .unwrap();

        let summary = summarize(&repo, &SummaryWindow::today()).unwrap();
        assert_eq!(
            summary.unknown_error_codes(),
            vec!["SOME_UNDECLARED_CODE"],
            "未登记的码要看得见"
        );
        assert!(
            summary.by_disposition.is_empty(),
            "未登记的码不该被硬套一个处置动作：{:?}",
            summary.by_disposition
        );
        assert!(summary.error_label("SOME_UNDECLARED_CODE").contains("未登记"));
        assert!(summary.error_disposition("SOME_UNDECLARED_CODE").is_none());
    }

    #[test]
    fn 渲染含关键字段与中文说明() {
        let repo = Repo::open_in_memory().unwrap();
        inject(&repo, 7, 3);
        let summary = summarize(&repo, &SummaryWindow::today()).unwrap();

        let line = summary.render();
        assert!(line.contains("processed=10"), "{}", line);
        assert!(line.contains("failed=3"), "{}", line);
        assert!(line.contains("failed_rate=0.300"), "{}", line);
        // 解析失败是终态，不占「待确认」的名额：只有 7 份好报告在等人点头。
        assert!(line.contains("awaiting_review=7"), "{}", line);
        assert!(line.contains("error_codes=UNIT_MISSING:3"), "{}", line);

        let report = summary.render_report();
        assert!(report.contains("整份没解析出来：3 份"), "{}", report);
        assert!(report.contains("30.0%"), "{}", report);
        assert!(report.contains("UNIT_MISSING"), "{}", report);
        assert!(report.contains("待修写法"), "{}", report);
        assert!(report.contains("当前状态分布"), "{}", report);
    }

    #[test]
    fn 日期格式不对显式失败不猜() {
        assert!(SummaryWindow::day("2026-13-01").is_none(), "月份非法");
        assert!(SummaryWindow::day("2026-01-32").is_none(), "日期非法");
        assert!(SummaryWindow::day("20260101").is_none(), "格式不对");
        assert!(SummaryWindow::day("2026/01/01").is_none(), "分隔符不对");
        let ok = SummaryWindow::day("2026-01-01").expect("合法日期");
        assert_eq!(ok.start(), Some("2026-01-01 00:00:00"));
        assert_eq!(ok.end(), Some("2026-01-02 00:00:00"));
        // 闰年 2 月 28 日 → 3 月 1 日。
        let leap = SummaryWindow::day("2024-02-28").expect("合法日期");
        assert_eq!(leap.end(), Some("2024-02-29 00:00:00"));
        let leap2 = SummaryWindow::day("2024-02-29").expect("合法日期");
        assert_eq!(leap2.end(), Some("2024-03-01 00:00:00"));
    }

    #[test]
    fn 无界窗口覆盖全部() {
        let repo = Repo::open_in_memory().unwrap();
        inject(&repo, 2, 1);
        let summary = summarize(&repo, &SummaryWindow::all()).unwrap();
        assert_eq!(summary.processed, 3);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.window_label, "全部");
    }
}
