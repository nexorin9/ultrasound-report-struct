//! Task 42 的集成验收：**命令行面**的装配与运维通道。
//!
//! 这一层要守住三件事：
//!
//! 1. **命令面完整**：`init-db / daemon / parse / ingest / export / replay / selftest`
//!    七个子命令都在，`--help` 打得开、用法说得清；
//! 2. **用法错误不装糊涂**：参数用错时**退出码非 0**，
//!    并把「错在哪、怎么改」说出来（不是 panic、不是静默兜底）；
//! 3. **运维通道真能用**：`replay` 重放不产生第二条台账行、
//!    `--apply` 补处理是**刷新**不是追加；`selftest` 一条命令出三个可核对数字，
//!    缺环境变量时**明确提示**而不是 panic。
//!
//! 全部用**真的可执行文件**跑（不是函数调用）—— 命令面的问题
//! （参数解析、退出码、cwd 相对路径）只有真进程才暴露得出来。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 被测可执行文件（cargo 为集成测试注入）。
const BIN: &str = env!("CARGO_BIN_EXE_ultrasound-report-struct");

/// 仓库根目录（资产与词典都在这儿；命令要在项目根跑）。
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "urs-cli-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 跑一次命令（在项目根目录下）。
fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(project_root())
        .output()
        .expect("应能跑起被测可执行文件")
}

/// 跑一次命令（**清空环境变量** —— 用来验「缺凭据时的提示」）。
fn run_without_env(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(project_root())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("应能跑起被测可执行文件")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// 命令行可执行文件里的状态库行数。
fn ledger_rows(db: &Path) -> i64 {
    let repo = ultrasound_report_struct::state::repo::Repo::open(db).expect("状态库应打得开");
    repo.connection()
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap()
}

/// 状态库里的报告行数。
fn report_rows(db: &Path) -> i64 {
    let repo = ultrasound_report_struct::state::repo::Repo::open(db).expect("状态库应打得开");
    repo.connection()
        .query_row("SELECT COUNT(*) FROM reports", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn 总帮助列出全部子命令() {
    let out = run(&["--help"]);
    assert!(out.status.success(), "--help 应退出码 0，实际 {:?}", out.status);
    let text = stdout_of(&out);
    for sub in [
        "init-db",
        "daemon",
        "parse",
        "ingest",
        "export",
        "replay",
        "selftest",
    ] {
        assert!(text.contains(sub), "帮助里应有子命令 {}：\n{}", sub, text);
    }
    // 全局参数三个都在。
    assert!(text.contains("--config"));
    assert!(text.contains("--state-db"));
    assert!(text.contains("--dry-run"));
}

#[test]
fn 各子命令的_帮助都打得开且退出码为0() {
    for sub in [
        "init-db",
        "parse",
        "ingest",
        "export",
        "summary",
        "replay",
        "selftest",
        "daemon",
        "ris-history",
    ] {
        let out = run(&[sub, "--help"]);
        assert!(
            out.status.success(),
            "{} --help 应退出码 0，实际 {:?}（stderr：{}）",
            sub,
            out.status,
            stderr_of(&out)
        );
        assert!(
            stdout_of(&out).contains("用法："),
            "{} --help 应打出用法",
            sub
        );
    }
}

#[test]
fn 用法错误一律非零退出且说清怎么改() {
    // 未知子命令。
    let out = run(&["no-such-command"]);
    assert_eq!(out.status.code(), Some(2), "未知子命令应退出码 2");
    assert!(stderr_of(&out).contains("未知子命令"));

    // 缺必填参数。
    for (args, keyword) in [
        (vec!["parse"], "--text"),
        (vec!["ingest"], "--inbox"),
        (vec!["export"], "--out"),
        (vec!["replay"], "--inbox"),
        (vec!["ris-history"], "--patient"),
    ] {
        let out = run(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{:?} 缺参数应退出码 2，实际 {:?}",
            args,
            out.status
        );
        assert!(
            stderr_of(&out).contains(keyword),
            "{:?} 的报错应点名缺哪个参数（{}），实际：{}",
            args,
            keyword,
            stderr_of(&out)
        );
    }

    // 全局参数缺取值。
    let out = run(&["--config"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("--config 缺少取值"));

    // 互斥参数。
    let out = run(&["summary", "--date", "2026-01-01", "--yesterday"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("互斥"));
}

#[test]
fn 守护进程拒绝_演练模式并指路() {
    let out = run(&["daemon", "--dry-run"]);
    assert!(
        !out.status.success(),
        "daemon --dry-run 应非零退出（常驻形态与「不落库」冲突）"
    );
    let err = stderr_of(&out);
    assert!(err.contains("--dry-run"), "报错应点名 --dry-run：{}", err);
    assert!(
        err.contains("ingest --inbox"),
        "报错应指路到演练用的命令：{}",
        err
    );
}

#[test]
fn 自检一条命令出三个可核对数字且退出码为0() {
    let out = run(&["selftest"]);
    assert!(
        out.status.success(),
        "selftest 应退出码 0，实际 {:?}（stderr：{}）",
        out.status,
        stderr_of(&out)
    );
    let text = stdout_of(&out);
    assert!(text.contains("台账行数"), "应打印台账行数：\n{}", text);
    assert!(text.contains("失败条目数"), "应打印失败条目数：\n{}", text);
    assert!(text.contains("差值条目数"), "应打印差值条目数：\n{}", text);
    assert!(
        text.contains("自检结论：通过"),
        "自检应全过：\n{}",
        text
    );
    // 三个数字都要是具体值，不是占位。
    assert!(text.contains("台账行数：1 行"), "\n{}", text);
    assert!(text.contains("失败条目数：1 条"), "\n{}", text);
    assert!(text.contains("差值条目数：16 条"), "\n{}", text);
}

#[test]
fn 自检缺环境变量时明确提示而不是panic() {
    let out = run_without_env(&["selftest"]);
    assert!(
        out.status.success(),
        "缺凭据不应判自检失败，实际 {:?}（stderr：{}）",
        out.status,
        stderr_of(&out)
    );
    let text = stdout_of(&out);
    assert!(
        text.contains("未配置出站凭据"),
        "应明确提示缺哪些凭据：\n{}",
        text
    );
    assert!(text.contains("RIS_API_TOKEN"));
    assert!(
        !text.contains("panicked") && !stderr_of(&out).contains("panicked"),
        "缺凭据不得 panic"
    );
}

#[test]
fn 自检资产缺失时明确报错且非零退出() {
    let out = run(&["selftest", "--assets", "/nonexistent-assets-for-cli-test"]);
    assert!(
        !out.status.success(),
        "资产缺失应非零退出，实际 {:?}",
        out.status
    );
    let err = stderr_of(&out);
    assert!(err.contains("找不到内置资产"), "应说清缺什么：{}", err);
    assert!(!err.contains("panicked"), "资产缺失不得 panic：{}", err);
}

#[test]
fn 演练模式不落状态库() {
    let dir = temp_dir("dry-run");
    let db = dir.join("state.sqlite");

    // init-db --dry-run：库不该被建出来。
    let out = run(&["init-db", "--state-db", db.to_str().unwrap(), "--dry-run"]);
    assert!(out.status.success());
    assert!(stdout_of(&out).contains("演练模式"));
    assert!(!db.exists(), "演练模式不该建出状态库");

    // 真建库，再演练入库：台账行数不变。
    let out = run(&["init-db", "--state-db", db.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let before = ledger_rows(&db);

    let out = run(&[
        "--state-db",
        db.to_str().unwrap(),
        "ingest",
        "--inbox",
        "assets/samples/inbox",
        "--dry-run",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("演练模式"), "\n{}", text);
    assert!(text.contains("将落 1 条台账行"), "\n{}", text);
    assert_eq!(ledger_rows(&db), before, "演练模式不得改台账");

    // 真入库：台账行数 +1。
    let out = run(&[
        "--state-db",
        db.to_str().unwrap(),
        "ingest",
        "--inbox",
        "assets/samples/inbox",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(ledger_rows(&db), before + 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 重放不产生第二条台账行且补处理是刷新不是追加() {
    let dir = temp_dir("replay");
    let db = dir.join("state.sqlite");

    let out = run(&["init-db", "--state-db", db.to_str().unwrap()]);
    assert!(out.status.success());
    let out = run(&[
        "--state-db",
        db.to_str().unwrap(),
        "ingest",
        "--inbox",
        "assets/samples/inbox",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));

    let items_before = ledger_rows(&db);
    let reports_before = report_rows(&db);
    assert_eq!(items_before, 1);

    // 对账式重放：口径没漂 → 一致；台账一个字节都不动。
    let out = run(&[
        "--state-db",
        db.to_str().unwrap(),
        "replay",
        "--inbox",
        "assets/samples/inbox",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("一致 2 份"), "\n{}", text);
    assert!(text.contains("漂移 0 份"), "\n{}", text);
    assert_eq!(ledger_rows(&db), items_before, "对账式重放不得改台账");
    assert_eq!(report_rows(&db), reports_before);

    // 把台账行改坏（模拟旧口径产物），再对账 → 应报漂移、指出差在哪。
    {
        let repo = ultrasound_report_struct::state::repo::Repo::open(&db).unwrap();
        repo.connection()
            .execute("UPDATE items SET value_si = 99.0", [])
            .unwrap();
    }
    let out = run(&[
        "--state-db",
        db.to_str().unwrap(),
        "replay",
        "--inbox",
        "assets/samples/inbox",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("漂移 1 份"), "\n{}", text);
    assert!(text.contains("[漂移]"), "应逐条指出漂移：\n{}", text);
    assert!(text.contains("99"), "应指出旧值：\n{}", text);
    assert_eq!(ledger_rows(&db), items_before, "对账式重放仍不得改台账");

    // 补处理：刷新成重算结果 —— **台账行数与报告行数都不变**。
    let out = run(&[
        "--state-db",
        db.to_str().unwrap(),
        "replay",
        "--inbox",
        "assets/samples/inbox",
        "--apply",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("补处理 1 份"), "\n{}", text);
    assert_eq!(
        ledger_rows(&db),
        items_before,
        "补处理是刷新，不得产生第二条台账行"
    );
    assert_eq!(
        report_rows(&db),
        reports_before,
        "补处理不得新增报告行"
    );
    // 值被刷回 32（不是 99）。
    let repo = ultrasound_report_struct::state::repo::Repo::open(&db).unwrap();
    let value: Option<f64> = repo
        .connection()
        .query_row("SELECT value_si FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, Some(32.0), "补处理应把值刷回重算结果");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 重放对库里没有的报告判为非重放对象() {
    let dir = temp_dir("replay-missing");
    let db = dir.join("state.sqlite");

    let out = run(&["init-db", "--state-db", db.to_str().unwrap()]);
    assert!(out.status.success());
    let out = run(&[
        "--state-db",
        db.to_str().unwrap(),
        "replay",
        "--inbox",
        "assets/samples/inbox",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("非重放对象 2 份"), "\n{}", text);
    assert!(text.contains("重放 0 份"), "\n{}", text);
    assert_eq!(ledger_rows(&db), 0, "非重放对象不得落台账行");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 待回写补处理在_emr_不可用时如实留在待回写() {
    let dir = temp_dir("retry-pending");
    let db = dir.join("state.sqlite");

    let out = run(&["init-db", "--state-db", db.to_str().unwrap()]);
    assert!(out.status.success());
    let out = run(&[
        "--state-db",
        db.to_str().unwrap(),
        "ingest",
        "--inbox",
        "assets/samples/inbox",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));

    // 把那份有台账行的报告置成「待回写」（模拟 EMR 那一步当时没写成）。
    {
        let repo = ultrasound_report_struct::state::repo::Repo::open(&db).unwrap();
        repo.connection()
            .execute(
                "UPDATE reports SET status = 'pending_writeback' WHERE study_no = 'sample-uro-001'",
                [],
            )
            .unwrap();
    }
    let items_before = ledger_rows(&db);

    // 本环境没有真实 EMR 凭据 → 补处理会失败，但**状态如实留在**待回写、
    // **台账行一行都不丢**，并且把失败说出来（不静默）。
    let out = run(&[
        "--state-db",
        db.to_str().unwrap(),
        "replay",
        "--retry-pending",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("候选 1 份"), "\n{}", text);
    assert!(text.contains("[补处理]"), "应逐份说明：\n{}", text);
    assert_eq!(ledger_rows(&db), items_before, "补处理失败不得丢台账行");

    let repo = ultrasound_report_struct::state::repo::Repo::open(&db).unwrap();
    let status: String = repo
        .connection()
        .query_row(
            "SELECT status FROM reports WHERE study_no = 'sample-uro-001'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "pending_writeback",
        "补处理失败后状态应如实留在待回写"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
