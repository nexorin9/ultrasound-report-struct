//! 主干切片 B 的集成验收：状态库 schema → 台账落库 → 台账 / 失败清单 CSV 导出。
//!
//! 这条链路是本项目的**主干**：一份脱敏报告投进投递目录 → 解析 → 强转 → 落台账行
//! → 导出可核对的台账 CSV 与带行列的失败清单 CSV。
//!
//! 断言口径与 `spec.md` §3 验收故事一致：
//! - 台账行：`3.2cm → 32mm`，强转路径 `UnitConvert`；
//! - 失败条目：单位缺失，带 **1 基行列**，指回原文。

use std::path::{Path, PathBuf};

use ultrasound_report_struct::export::export_all;
use ultrasound_report_struct::ingest::ingest_inbox;
use ultrasound_report_struct::state::repo::Repo;
use ultrasound_report_struct::state::schema::SCHEMA_VERSION;

fn sample_inbox() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/samples/inbox")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "urs-state-export-{}-{}",
        tag,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn read_rows(path: &Path) -> Vec<String> {
    let content = std::fs::read_to_string(path).unwrap();
    content
        .lines()
        .map(|l| l.trim_start_matches('\u{feff}').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

#[test]
fn 状态库建库后投递批次产出可核对台账与失败清单() {
    let dir = temp_dir("main");
    let db = dir.join("urs.db");
    let out = dir.join("export");

    // 1) 建库（幂等迁移）。
    let repo = Repo::open(&db).unwrap();
    let version: i64 = repo
        .connection()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION, "schema 版本应为当前版本");

    // 2) 投递批次：两份样例（一份带单位、一份缺单位）。
    let summary = ingest_inbox(&repo, &sample_inbox()).unwrap();
    assert_eq!(summary.reports, 2, "两份样例报告都应入库");
    assert_eq!(summary.items, 1, "只有带单位那份产出结构化条目");
    assert_eq!(summary.failures, 1, "缺单位那份产出 1 条失败条目");

    // 3) 导出。
    let exported = export_all(repo.connection(), &out).unwrap();
    assert_eq!(exported.ledger_rows, 1);
    assert_eq!(exported.failure_rows, 1);

    // 4) 台账行断言：3.2cm → 32mm，走 UnitConvert。
    let ledger = read_rows(&exported.ledger_path);
    assert_eq!(ledger.len(), 2, "表头 + 1 行台账");
    assert!(ledger[0].contains("coercion_path"), "表头：{}", ledger[0]);
    let row = &ledger[1];
    assert!(row.contains("sample-uro-001"), "台账行：{}", row);
    assert!(row.contains("kidney.left"), "台账行：{}", row);
    assert!(row.contains(",32,"), "归一值应为 32：{}", row);
    assert!(row.contains(",mm,"), "单位码应为 mm：{}", row);
    assert!(row.contains("UnitConvert"), "强转路径：{}", row);

    // 5) 失败清单断言：带 1 基行列。
    let failures = read_rows(&exported.failure_path);
    assert_eq!(failures.len(), 2, "表头 + 1 行失败条目");
    let fail_row = &failures[1];
    assert!(fail_row.contains("sample-uro-002"), "失败行：{}", fail_row);
    assert!(fail_row.contains("UNIT_MISSING"), "失败行：{}", fail_row);
    let cols: Vec<&str> = fail_row.split(',').collect();
    assert_eq!(cols.len(), 5, "失败行应有 5 列：{}", fail_row);
    let line: i64 = cols[2].parse().expect("行号应为数字");
    let column: i64 = cols[3].parse().expect("列号应为数字");
    assert!(line >= 1, "行号应为 1 基");
    assert!(column >= 1, "列号应为 1 基");
    assert_eq!(cols[4], "3.2", "原文片段应回显原值");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 重复投递不产生第二条台账行() {
    let dir = temp_dir("idem");
    let db = dir.join("urs.db");
    let out = dir.join("export");
    let repo = Repo::open(&db).unwrap();

    let first = ingest_inbox(&repo, &sample_inbox()).unwrap();
    assert_eq!(first.reports, 2);
    let second = ingest_inbox(&repo, &sample_inbox()).unwrap();
    assert_eq!(second.reports, 0, "重复投递不得再入库");
    assert_eq!(second.duplicates, 2, "两份都应命中幂等预查");

    let exported = export_all(repo.connection(), &out).unwrap();
    assert_eq!(exported.ledger_rows, 1, "台账行数不变");
    assert_eq!(exported.failure_rows, 1, "失败条目数不变");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 重复执行建库迁移不报错也不重复建表() {
    let dir = temp_dir("migrate");
    let db = dir.join("urs.db");

    let repo = Repo::open(&db).unwrap();
    drop(repo);
    let repo2 = Repo::open(&db).unwrap();
    let version: i64 = repo2
        .connection()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION, "重复建库后版本号不变");

    let tables: i64 = repo2
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' \
             AND name IN ('reports','items','comparisons','audit_log','idempotency')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 5, "五张表各一份");

    std::fs::remove_dir_all(&dir).ok();
}
