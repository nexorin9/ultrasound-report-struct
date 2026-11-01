//! Task 8 的集成验收：目录轮询监听 + 写入稳定检测（去抖）+ 幂等预查。
//!
//! 与单元测试的差别：这里**不用注入时钟**，走**真实 1 秒 tick**；
//! 状态库是**落盘的真实 SQLite**（不是内存库）；语料是仓库里真实的脱敏样例。
//! 断言口径与 `spec.md` §3 验收故事一致：
//! - 监听器从投递目录发现 `inbox/<检查号>/report.txt`，取到的是**完整**正文；
//! - 同一份报告再投一次 → 幂等预查命中 → 跳过并写审计，**不产生第二条台账行**。

use std::path::{Path, PathBuf};
use std::time::Duration;

use ultrasound_report_struct::ingest::watcher::{Watcher, REPORT_TXT};
use ultrasound_report_struct::log::Logger;
use ultrasound_report_struct::parse::parse_report;
use ultrasound_report_struct::state::repo::{IngestRecord, Repo};
use ultrasound_report_struct::state::idempotency_key;

/// 仓库里的真实脱敏样例投递目录。
fn sample_inbox() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/samples/inbox")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "urs-watcher-integration-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 把仓库样例复制进临时投递目录（模拟工作站投递）。
fn deliver(inbox: &Path, study_no: &str, sample: &str) -> PathBuf {
    let src = sample_inbox().join(sample).join(REPORT_TXT);
    let dst_dir = inbox.join(study_no);
    std::fs::create_dir_all(&dst_dir).unwrap();
    let dst = dst_dir.join(REPORT_TXT);
    std::fs::copy(&src, &dst).unwrap();
    dst
}

fn count(repo: &Repo, table: &str) -> i64 {
    repo.connection()
        .query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn 监听器发现真实样例并拦截重复投递() {
    let dir = temp_dir("discover");
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let report_path = deliver(&inbox, "sample-uro-001", "sample-uro-001");

    // 落盘的真实状态库（不是内存库）。
    let repo = Repo::open(&dir.join("state.sqlite")).unwrap();
    let logger = Logger::stdout();
    let mut watcher = Watcher::new(&inbox);

    // 第 1 轮：首见 —— 还没有第二次采样，不交付。
    let out = watcher.poll_once(&repo, &logger).unwrap();
    assert!(out.candidates.is_empty(), "首见不得交付");
    assert_eq!(out.pending, 1);

    // 等一个 tick（>1 秒）后第 2 轮：大小与修改时间都没变 → 稳定，可取件。
    std::thread::sleep(Duration::from_millis(1100));
    let out = watcher.poll_once(&repo, &logger).unwrap();
    assert_eq!(out.candidates.len(), 1, "稳定后应取出候选");
    let candidate = &out.candidates[0];
    assert_eq!(candidate.study_no, "sample-uro-001");
    assert_eq!(candidate.path, report_path);

    // 取到的是**完整**正文（不是半截），且摘要与入库路径的幂等键同源。
    let text = candidate.text().expect("样例是 UTF-8");
    assert!(text.contains("左肾积水 3.2cm"), "取到的应是完整正文：{}", text);
    assert_eq!(
        candidate.idempotency_key,
        idempotency_key("sample-uro-001", text),
        "预查摘要必须与入库路径的幂等键同源"
    );

    // 走真实入库路径（解析 → 强转 → 落台账行）。
    let outcome = parse_report(text);
    assert_eq!(outcome.items.len(), 1);
    assert_eq!(outcome.items[0].value_si, Some(32.0));
    assert_eq!(outcome.items[0].unit_code.as_deref(), Some("mm"));
    repo.commit_ingestion(&IngestRecord {
        study_no: "sample-uro-001",
        patient_id: Some("PID-EXAMPLE"),
        source_path: Some(&report_path.to_string_lossy()),
        idempotency_key: &candidate.idempotency_key,
        dictionary_version: ultrasound_report_struct::dictionary_version(),
        items: &outcome.items,
        failures: &outcome.failures,
    })
    .unwrap();
    assert_eq!(count(&repo, "items"), 1);

    // 工作站重投同一份（工作站重发 / 人工重投的共同场景）：
    // 重写同样内容 = 修改时间变化（工作站重新导出），监听器当它是一份新投递重新判定。
    std::fs::write(&report_path, text).unwrap();
    std::thread::sleep(Duration::from_millis(1100));

    // 刚被重写的那一轮：内容在变 → 本轮跳过，等它静默下来。
    let out = watcher.poll_once(&repo, &logger).unwrap();
    assert!(out.candidates.is_empty(), "刚被重写的文件不得立刻交付");
    assert_eq!(out.growing, 1);

    // 静默一轮后：幂等预查命中 → 跳过并写审计，**不产生第二条台账行**。
    std::thread::sleep(Duration::from_millis(1100));
    let out = watcher.poll_once(&repo, &logger).unwrap();
    assert!(out.candidates.is_empty(), "重复投递不得再交付");
    assert_eq!(out.duplicates, 1);
    assert_eq!(count(&repo, "items"), 1, "重复投递不得产生第二条台账行");
    assert_eq!(count(&repo, "reports"), 1);

    let (action, digest): (String, String) = repo
        .connection()
        .query_row(
            "SELECT action, input_digest FROM audit_log WHERE action = 'duplicate_skipped'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(action, "duplicate_skipped");
    assert_eq!(digest, candidate.idempotency_key);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 仍在写入的报告不被抢读() {
    let dir = temp_dir("growing");
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();

    // 先投半截（模拟工作站正在写）。
    let full = std::fs::read_to_string(
        sample_inbox().join("sample-uro-002").join(REPORT_TXT),
    )
    .unwrap();
    let half = &full[..full.len() / 2];
    let report_dir = inbox.join("sample-uro-002");
    std::fs::create_dir_all(&report_dir).unwrap();
    let report_path = report_dir.join(REPORT_TXT);
    std::fs::write(&report_path, half).unwrap();

    let repo = Repo::open(&dir.join("state.sqlite")).unwrap();
    let logger = Logger::stdout();
    let mut watcher = Watcher::new(&inbox);

    // 第 1 轮：首见。
    let out = watcher.poll_once(&repo, &logger).unwrap();
    assert!(out.candidates.is_empty());
    assert_eq!(out.pending, 1);

    // 工作站把剩下的写完（大小变了）。
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(&report_path, &full).unwrap();

    // 第 2 轮：大小变了 → 仍在写，本轮跳过（**不抢读半截正文**）。
    let out = watcher.poll_once(&repo, &logger).unwrap();
    assert!(out.candidates.is_empty(), "仍在写入的报告不得被抢读");
    assert_eq!(out.growing, 1);

    // 第 3 轮：大小稳定且已静默 ≥1 秒 → 取到完整正文。
    std::thread::sleep(Duration::from_millis(1100));
    let out = watcher.poll_once(&repo, &logger).unwrap();
    assert_eq!(out.candidates.len(), 1);
    assert_eq!(
        out.candidates[0].byte_len,
        full.len() as u64,
        "必须取到完整正文，而不是半截"
    );
    assert!(out.candidates[0].text().unwrap().contains("提示：左肾积水"));

    std::fs::remove_dir_all(&dir).ok();
}
