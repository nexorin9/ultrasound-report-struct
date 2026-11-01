//! **并发打开同一个状态库**（批处理与守护进程重叠启动时的真实场景）。
//!
//! ## 这一层要挡住什么
//!
//! 守护进程起 4 个工作线程，每条线程自己开一条连接；运维又可能在 crontab 里
//! 加一趟批量补处理 —— 两个进程**同时**打开同一个状态库。
//!
//! 打开一个库要跑**迁移**（建表 + 索引 + 版本号）。迁移是写操作，
//! 两个进程同时迁同一个新库就会撞写锁：
//!
//! | 撞锁时的表现 | 后果 |
//! |---|---|
//! | 直接报 `database is locked` | **守护进程起不来**（或批处理整趟失败） |
//! | 静默跳过迁移 | 表建了一半，报告写不进去 |
//!
//! 所以开库这一层必须做到：**并发打开同一个新库不失败、迁移只跑一次、
//! 版本号一致**。
//!
//! ## 为什么这不是「测试洁癖」
//!
//! 这是**并发保护三层防线的第 0 层** —— 库还没建好的时候，
//! 「同一检查号串行 / 不同检查号并行」根本谈不上。
//! 守护进程与批处理重叠启动正是本项目明确要覆盖的院内现实（见
//! `spec.md` §4「幂等 / 并发重试」）。

use ultrasound_report_struct::state::repo::Repo;
use ultrasound_report_struct::state::schema::SCHEMA_VERSION;

use std::path::PathBuf;
use std::sync::{Arc, Barrier};

/// 临时目录（每个用例一个，跑完清掉）。
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "urs-schema-open-{}-{}-{:?}",
        tag,
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// **并发打开同一个新状态库：全部成功，版本号一致，表建齐。**
///
/// 8 条线程同时开同一个**还不存在**的库 —— 迁移会被并发触发。
/// 修复前这条会间歇性报 `database is locked`（约三次里挂一次）。
#[test]
fn 并发打开同一个新状态库全部成功() {
    let dir = temp_dir("new-db");
    let db_path = dir.join("state.db");

    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut handles = Vec::new();
    for _ in 0..workers {
        let db_path = db_path.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            // 一起冲 —— 让并发窗口真的打开。
            barrier.wait();
            Repo::open(&db_path).map(|_| ()).map_err(|e| e.to_string())
        }));
    }

    let mut failures = Vec::new();
    for handle in handles {
        if let Err(msg) = handle.join().unwrap() {
            failures.push(msg);
        }
    }
    assert!(
        failures.is_empty(),
        "并发打开新状态库不应失败，实际 {} 条失败：{:?}",
        failures.len(),
        failures
    );

    // 迁移只跑一次、版本号一致、表建齐。
    let repo = Repo::open(&db_path).unwrap();
    let version: i64 = repo
        .connection()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION, "版本号应一致");

    let tables: String = repo
        .connection()
        .query_row(
            "SELECT group_concat(name, ',') FROM (SELECT name FROM sqlite_master \
             WHERE type='table' AND name IN \
             ('reports','items','comparisons','audit_log','idempotency','study_locks') \
             ORDER BY name)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        tables,
        "audit_log,comparisons,idempotency,items,reports,study_locks",
        "六张表都要建齐"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **并发打开一个已建好的库**：不重跑迁移，也不报错。
///
/// 已经迁完的库再开，迁移应当**直接跳过**（`applied = 0`）——
/// 不是「再跑一遍建表」而是「看一眼版本号就知道没事了」。
#[test]
fn 并发打开已建好的库不重跑迁移() {
    let dir = temp_dir("existing-db");
    let db_path = dir.join("state.db");

    // 先建好。
    let first = Repo::open(&db_path).unwrap();
    let applied: usize = ultrasound_report_struct::state::schema::migrate(first.connection())
        .unwrap();
    assert_eq!(applied, 0, "库已是最新版本，不该再跑迁移");

    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut handles = Vec::new();
    for _ in 0..workers {
        let db_path = db_path.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            Repo::open(&db_path)
                .map_err(|e| e.to_string())
                .and_then(|r| {
                    ultrasound_report_struct::state::schema::migrate(r.connection())
                        .map_err(|e| e.to_string())
                })
        }));
    }

    for handle in handles {
        match handle.join().unwrap() {
            Ok(applied) => assert_eq!(applied, 0, "已建好的库不该重跑迁移"),
            Err(msg) => panic!("并发打开已建好的库不应失败：{}", msg),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
