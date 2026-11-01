//! 工作流第 5 层验收：**运行摘要**（回答「昨天有多少报告没解析出来」）。
//!
//! 验收的是运维真正会走的两条路：
//!
//! 1. **库级**：10 份报告（3 份坏件）走**真实入库路径**（解析 → 强转 → 落库 → 留痕），
//!    摘要的口径逐项断言 —— 受理 10 份 / 整份没解析出来 3 份 / 失败率 0.3 /
//!    失败条目按错误码聚合；
//! 2. **命令行**：起**真的可执行文件**跑 `summary`，断言它打出来的简报与退出码 ——
//!    值班的人不会去调库，他敲一条命令。
//!
//! **替身说明**：这里没有外部系统替身 —— RIS / EMR / 企微三条出站在本轮尚未接入，
//! 摘要只看**状态库**，不依赖它们。

use std::path::{Path, PathBuf};
use std::process::Command;

use ultrasound_report_struct::daemon::processor::{IngestProcessor, ReportProcessor};
use ultrasound_report_struct::ingest::watcher::{Candidate, CandidateKind};
use ultrasound_report_struct::state::idempotency_key;
use ultrasound_report_struct::state::repo::Repo;
use ultrasound_report_struct::workflow::summary::{summarize, SummaryWindow};

/// 被测可执行文件（cargo 为集成测试注入）。
const BIN: &str = env!("CARGO_BIN_EXE_ultrasound-report-struct");

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "urs-workflow-summary-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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

/// 投递 7 份好报告与 3 份坏件（缺单位），走真实入库路径。
fn deliver(repo: &Repo, good: usize, bad: usize) {
    for i in 0..good {
        let body = format!(
            "超声所见：\n左肾前后径 3.2cm\n膀胱残余尿量 约 {}ml",
            100 + i
        );
        let out = IngestProcessor
            .process(repo, &candidate(&format!("S-SUM-GOOD-{:03}", i), &body))
            .expect("好报告应处理成功");
        assert_eq!(out.failures, 0, "好报告不该有失败条目");
    }
    for i in 0..bad {
        let body = format!("超声所见：\n右肾积水 {}.1", i + 1);
        let out = IngestProcessor
            .process(repo, &candidate(&format!("S-SUM-BAD-{:03}", i), &body))
            .expect("坏件应处理成功（落失败条目）");
        assert_eq!(out.failures, 1, "坏件应落一条失败条目");
    }
}

#[test]
fn 库级摘要按状态与错误码聚合计数() {
    let dir = temp_dir("repo");
    let db = dir.join("state.sqlite");
    let repo = Repo::open(&db).unwrap();
    deliver(&repo, 7, 3);

    let summary = summarize(&repo, &SummaryWindow::today()).unwrap();

    // 运维问的那句话：「昨天有多少报告没解析出来」。
    assert_eq!(summary.processed, 10, "受理 10 份");
    assert_eq!(summary.failed, 3, "整份没解析出来 3 份");
    assert!((summary.failed_rate() - 0.3).abs() < 1e-9, "失败率 0.3");
    assert_eq!(summary.failed_items, 3, "失败条目 3 条");
    assert_eq!(summary.duplicates, 0);

    // 按状态：7 份待确认（出条目了还没人确认）+ 3 份解析失败（终态）。
    assert_eq!(summary.by_status.get("parsed").copied(), Some(7));
    assert_eq!(summary.by_status.get("parse_failed").copied(), Some(3));
    assert_eq!(summary.awaiting_review, 7);
    assert_eq!(summary.pending_writeback, 0);

    // 按错误码 + 处置动作：3 份都是缺单位 → 待修写法。
    assert_eq!(summary.by_error_code.get("UNIT_MISSING").copied(), Some(3));
    assert_eq!(
        summary.by_disposition.get("AWAITING_VALUE").copied(),
        Some(3)
    );
    assert!(
        summary.unknown_error_codes().is_empty(),
        "失败必须收敛到稳定码表：{:?}",
        summary.unknown_error_codes()
    );

    // 摘要里的每个数都能在库里对上（不是凭空算的）。
    let reports: i64 = repo
        .connection()
        .query_row("SELECT COUNT(*) FROM reports", [], |row| row.get(0))
        .unwrap();
    assert_eq!(reports, 10);
    let failed_audit: i64 = repo
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'failed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(failed_audit, 3, "失败条目在审计留痕里也在");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 命令行摘要打出可核对简报() {
    let dir = temp_dir("cli");
    let db = dir.join("state.sqlite");
    let repo = Repo::open(&db).unwrap();
    deliver(&repo, 7, 3);
    drop(repo);

    // 真进程、真参数、真退出码。
    let output = Command::new(BIN)
        .args(["--state-db", db.to_str().unwrap(), "summary"])
        .output()
        .expect("应能起起 summary 子命令");
    assert!(
        output.status.success(),
        "summary 应正常退出，stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("受理报告：10 份"), "{}", stdout);
    assert!(stdout.contains("整份没解析出来：3 份"), "{}", stdout);
    assert!(stdout.contains("30.0%"), "{}", stdout);
    assert!(stdout.contains("UNIT_MISSING"), "{}", stdout);
    assert!(stdout.contains("待修写法"), "{}", stdout);
    assert!(stdout.contains("当前状态分布"), "{}", stdout);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 日期不合法显式失败不猜() {
    let dir = temp_dir("baddate");
    let db = dir.join("state.sqlite");
    Repo::open(&db).unwrap();

    let output = Command::new(BIN)
        .args([
            "--state-db",
            db.to_str().unwrap(),
            "summary",
            "--date",
            "2026-13-01",
        ])
        .output()
        .expect("应能起起 summary 子命令");
    assert!(
        !output.status.success(),
        "日期不合法必须非零退出（不能猜是哪一天）"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("YYYY-MM-DD"),
        "报错要说清日期格式：{}",
        stderr
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 状态库不存在时显式报错不凭空建空库() {
    let dir = temp_dir("nodB");
    let missing = dir.join("not-created.sqlite");

    let output = Command::new(BIN)
        .args(["--state-db", missing.to_str().unwrap(), "summary"])
        .output()
        .expect("应能起起 summary 子命令");
    assert!(!output.status.success(), "状态库不存在应非零退出");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("状态库不存在"),
        "报错要说清状态库不存在：{}",
        stderr
    );
    assert!(
        !Path::new(&missing).exists(),
        "不该凭空建一个空状态库再报「一切正常」"
    );

    std::fs::remove_dir_all(&dir).ok();
}
