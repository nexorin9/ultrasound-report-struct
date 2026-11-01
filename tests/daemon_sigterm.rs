//! Task 9 的集成验收：**真实进程**的 SIGTERM 优雅退出。
//!
//! 与单元测试的差别：这里起的是**真的可执行文件**，用**真的信号**（`kill -TERM`），
//! 断言的是**真的退出码** —— 这是「事件驱动守护」形态能不能上机的最后一环：
//! 值班机上 `systemctl stop` / `Ctrl-C` 之后，进程必须
//! **停在新任务之前 → 等在途任务跑完 → 落库 → 退出码 0**。
//!
//! 凭据说明：本任务尚未接入 RIS / EMR / 企微三条出站（Task 28 / 38 / 39），
//! 这里给的是**占位值**，只用于通过启动自检的守护档校验；进程不会真的外发请求。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ultrasound_report_struct::state::repo::Repo;

/// 被测可执行文件（cargo 为集成测试注入）。
const BIN: &str = env!("CARGO_BIN_EXE_ultrasound-report-struct");

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "urs-daemon-sigterm-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 写一份配置文件 + 最小词典（启动自检要求词典文件存在且可解析）。
///
/// **两本词典都用仓库里真实的那一份** —— 词典文件存在却坏了，守护进程会直接退出
/// （不退回内置旧词典），测试里必须放一份真词典，不能放占位 JSON。
fn write_config(dir: &Path) -> PathBuf {
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let dict = dir.join("dict");
    std::fs::create_dir_all(&dict).unwrap();
    let repo_parts = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/dictionary/parts.json");
    std::fs::copy(&repo_parts, dict.join("parts.json")).unwrap();
    let repo_items = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/dictionary/items.json");
    std::fs::copy(&repo_items, dict.join("items.json")).unwrap();

    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[inbox]\ndir = \"{}\"\ntick_interval_ms = 200\n\n\
             [state]\ndb = \"{}\"\n\n\
             [dictionary]\ndir = \"{}\"\nparts_file = \"{}/parts.json\"\nitems_file = \"{}/items.json\"\n\n\
             [outbound]\nmax_concurrency = 4\n",
            inbox.display(),
            dir.join("state.sqlite").display(),
            dict.display(),
            dict.display(),
            dict.display()
        ),
    )
    .unwrap();
    config_path
}

/// 起守护进程（stdout / stderr 落到文件，便于事后核对真实输出）。
fn spawn_daemon(config_path: &Path, log_path: &Path) -> Child {
    let log = std::fs::File::create(log_path).unwrap();
    let err_log = log_path.with_extension("err.log");
    let err = std::fs::File::create(&err_log).unwrap();
    Command::new(BIN)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "daemon",
            "--tick-ms",
            "200",
        ])
        .env("RIS_API_TOKEN", "placeholder-ris-token-for-startup-check")
        .env("EMR_API_TOKEN", "placeholder-emr-token-for-startup-check")
        .env(
            "WECOM_WEBHOOK_URL",
            "https://wecom.example.invalid/hook-placeholder",
        )
        .env("DAEMON_API_TOKEN", "placeholder-daemon-token-for-startup-check")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("守护进程应能启动")
}

/// 等子进程退出（带超时）。
fn wait_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    None
}

/// 台账行数（读真实落盘的状态库）。
fn ledger_rows(db: &Path) -> i64 {
    let repo = Repo::open(db).unwrap();
    repo.connection()
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn sigterm_后守护进程退出码为零且在途任务已落库() {
    let dir = temp_dir("sigterm");
    let config_path = write_config(&dir);
    let db = dir.join("state.sqlite");
    let log_path = dir.join("daemon.log");

    // 超声工作站投递一份报告。
    let study_dir = dir.join("inbox").join("S-SIGTERM-001");
    std::fs::create_dir_all(&study_dir).unwrap();
    std::fs::write(
        study_dir.join("report.txt"),
        "病历号：PID-EXAMPLE\n超声所见\n左肾积水 3.2cm\n",
    )
    .unwrap();

    let mut child = spawn_daemon(&config_path, &log_path);
    let pid = child.id();

    // 1) 等守护进程把这份报告处理完（真实落盘状态库）。
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut processed = false;
    while Instant::now() < deadline {
        if ledger_rows(&db) == 1 {
            processed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(processed, "守护进程应把投递的报告处理完并落台账行");

    // 2) 发真信号（SIGTERM）。
    let killed = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("应能发出 SIGTERM");
    assert!(killed.success(), "kill -TERM 应执行成功");

    // 3) 进程应**优雅退出**：退出码 0。
    let status = wait_exit(&mut child, Duration::from_secs(20))
        .expect("收到 SIGTERM 后进程应在 20 秒内退出");
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM 后应优雅退出（退出码 0），实际 {:?}",
        status
    );

    // 4) 在途任务已落库（退出后数据仍在）。
    assert_eq!(ledger_rows(&db), 1, "退出后台账行应仍在（不丢在途）");

    // 5) 真实日志里有停机事件与心跳。
    let mut text = String::new();
    std::fs::File::open(&log_path)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert!(text.contains("DAEMON_STARTED"), "{}", text);
    assert!(text.contains("DAEMON_HEARTBEAT"), "{}", text);
    assert!(text.contains("DAEMON_STOPPING"), "{}", text);
    assert!(text.contains("DAEMON_STOPPED"), "{}", text);
    assert!(
        text.contains("REPORT_PROCESSED"),
        "应有一条处理完成事件：{}",
        text
    );
    // 心跳里带运维要的那四个计数。
    let heartbeat = text
        .lines()
        .find(|line| line.contains("DAEMON_HEARTBEAT"))
        .expect("应有心跳行");
    for field in ["processed=", "failed=", "awaiting_review=", "pending_writeback="] {
        assert!(heartbeat.contains(field), "心跳应带 {}：{}", field, heartbeat);
    }

    let _ = std::fs::remove_dir_all(&dir);
}
