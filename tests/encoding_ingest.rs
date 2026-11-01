//! 端到端：编码判别在**真实入库链路**上的行为。
//!
//! 覆盖三条断言：
//! 1. **GBK 报告照常入库** —— 老设备投的 GBK 报告能解出正文、抽出测值、落台账行；
//! 2. **混编报告显式失败** —— 不产出条目，落一条带 `ENCODING_MIXED` 的失败条目；
//! 3. **非文本报告显式失败** —— 落一条带 `ENCODING_UNKNOWN` 的失败条目。
//!
//! 「显式失败」= 失败清单里有它、运行摘要按错误码能聚合到它 ——
//! 这一份报告**不能悄悄消失**（台账里没有行，但错误码在）。
//!
//! 最后一条用例起的是**真的守护进程**：事件到达即处理，
//! GBK 报告走同一条主路径落台账，混编报告带错误码落失败清单。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ultrasound_report_struct::export::export_all;
use ultrasound_report_struct::ingest::ingest_inbox;
use ultrasound_report_struct::state::repo::Repo;

/// 被测可执行文件（cargo 为集成测试注入）。
const BIN: &str = env!("CARGO_BIN_EXE_ultrasound-report-struct");

/// 临时工作目录（每个用例一个）。
fn work_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("urs-encoding-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 投递一份报告（字节原样写入，不做任何转码）。
fn deliver(inbox: &Path, study_no: &str, bytes: &[u8]) {
    let dir = inbox.join(study_no);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("report.txt"), bytes).unwrap();
}

/// 把一段中文文本编成 GBK 字节（用字符表自带的编码器，不手抄字节）。
fn gbk_bytes(text: &str) -> Vec<u8> {
    let (bytes, _, _) = encoding_rs::GBK.encode(text);
    bytes.into_owned()
}

/// 读导出的 CSV 数据行（去掉表头）。
fn csv_rows(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap();
    text.lines().skip(1).map(|l| l.to_string()).collect()
}

/// 查失败清单里的错误码。
fn failure_codes(rows: &[String]) -> Vec<String> {
    rows.iter()
        .map(|row| row.split(',').nth(1).unwrap_or("").to_string())
        .collect()
}

#[test]
fn gbk报告照常入库并落台账行() {
    let dir = work_dir("gbk");
    let inbox = dir.join("inbox");
    let db = dir.join("state.db");
    let out = dir.join("out");
    std::fs::create_dir_all(&inbox).unwrap();

    // 一份 GBK 的泌尿系报告（工作站老设备导出）。
    let body = "病历号：PID-EXAMPLE\n超声所见\n左肾积水 3.2cm\n";
    deliver(&inbox, "sample-gbk-001", &gbk_bytes(body));

    let repo = Repo::open(&db).unwrap();
    let summary = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(summary.reports, 1, "GBK 报告应照常入库");
    assert_eq!(summary.items, 1, "应抽出一条结构化条目");
    assert_eq!(summary.failures, 0);
    assert_eq!(summary.encoding_failed, 0, "GBK 不算编码失败");

    export_all(repo.connection(), &out).unwrap();
    let ledger = csv_rows(&out.join("ledger.csv"));
    assert_eq!(ledger.len(), 1, "台账应有 1 行");
    assert!(
        ledger[0].contains("sample-gbk-001") && ledger[0].contains("32") && ledger[0].contains("mm"),
        "GBK 报告应落出归一后的测值：{}",
        ledger[0]
    );
    assert_eq!(
        csv_rows(&out.join("failures.csv")).len(),
        0,
        "GBK 报告不应进失败清单"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 混编报告显式失败且不产出条目() {
    let dir = work_dir("mixed");
    let inbox = dir.join("inbox");
    let db = dir.join("state.db");
    let out = dir.join("out");
    std::fs::create_dir_all(&inbox).unwrap();

    // 前半段 GBK、后半段 UTF-8（模板与所见不是同一种编码）。
    let mut bytes = gbk_bytes("左肾积水 3.2cm\n");
    bytes.extend_from_slice("积水明显".as_bytes());
    deliver(&inbox, "sample-mixed-001", &bytes);

    let repo = Repo::open(&db).unwrap();
    let summary = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(summary.encoding_failed, 1, "混编应计入编码失败");
    assert_eq!(summary.items, 0, "混编不得产出条目");
    assert_eq!(summary.failures, 1, "混编应落 1 条失败条目");

    export_all(repo.connection(), &out).unwrap();
    let ledger = csv_rows(&out.join("ledger.csv"));
    assert!(ledger.is_empty(), "混编不得落台账行：{:?}", ledger);

    let failures = csv_rows(&out.join("failures.csv"));
    assert_eq!(failures.len(), 1, "失败清单应有 1 行");
    assert_eq!(
        failure_codes(&failures),
        vec!["ENCODING_MIXED".to_string()],
        "失败清单要带稳定错误码：{:?}",
        failures
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 非文本报告显式失败且不产出条目() {
    let dir = work_dir("garbage");
    let inbox = dir.join("inbox");
    let db = dir.join("state.db");
    let out = dir.join("out");
    std::fs::create_dir_all(&inbox).unwrap();

    // 二进制垃圾：UTF-8 与 GBK 都解不出。
    deliver(&inbox, "sample-bin-001", &[0xFF, 0xFE, 0x00, 0x01, 0x8F]);

    let repo = Repo::open(&db).unwrap();
    let summary = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(summary.encoding_failed, 1);
    assert_eq!(summary.items, 0, "不得产出条目");
    assert_eq!(summary.failures, 1, "应落 1 条失败条目");

    export_all(repo.connection(), &out).unwrap();
    assert!(csv_rows(&out.join("ledger.csv")).is_empty());
    let failures = csv_rows(&out.join("failures.csv"));
    assert_eq!(failure_codes(&failures), vec!["ENCODING_UNKNOWN".to_string()]);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 同一份坏件重复投递不产生第二条失败条目() {
    let dir = work_dir("dedup");
    let inbox = dir.join("inbox");
    let db = dir.join("state.db");
    let out = dir.join("out");
    std::fs::create_dir_all(&inbox).unwrap();

    let garbage = [0xFF, 0xFE, 0x00, 0x01, 0x8F];
    deliver(&inbox, "sample-bin-002", &garbage);

    let repo = Repo::open(&db).unwrap();
    let first = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(first.encoding_failed, 1);

    // 同一份坏件再投一次：幂等键命中，不产生第二条失败条目。
    let second = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(second.encoding_failed, 1);
    assert_eq!(second.duplicates, 1, "重复投递应计入重复跳过");

    export_all(repo.connection(), &out).unwrap();
    assert_eq!(
        csv_rows(&out.join("failures.csv")).len(),
        1,
        "重复投递不得产生第二条失败条目"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// 写一份配置文件 + 最小词典（启动自检要求词典文件存在且可解析）。
///
/// **部位词典用仓库里真实的那一份** —— 词典文件存在却坏了，守护进程会直接退出
/// （不退回内置旧词典），测试里必须放一份真词典，不能放占位 JSON。
fn write_config(dir: &Path) -> PathBuf {
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let dict = dir.join("dict");
    std::fs::create_dir_all(&dict).unwrap();
    let repo_parts = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/dictionary/parts.json");
    std::fs::copy(&repo_parts, dict.join("parts.json")).unwrap();
    // 测项词典同样用仓库里真实的那一份：词典文件存在却坏了，守护进程会直接退出。
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
    let err = std::fs::File::create(log_path.with_extension("err.log")).unwrap();
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

/// 读真实落盘状态库里的失败条目数（按错误码聚合）。
fn failure_count(db: &Path, error_code: &str) -> i64 {
    let repo = Repo::open(db).unwrap();
    repo.connection()
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'failed' AND error_code = ?1",
            [error_code],
            |row| row.get(0),
        )
        .unwrap()
}

/// 台账行数（读真实落盘的状态库）。
fn item_rows(db: &Path) -> i64 {
    let repo = Repo::open(db).unwrap();
    repo.connection()
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn 守护进程实跑_gbk落台账_混编带错误码落失败清单() {
    let dir = work_dir("daemon");
    let config_path = write_config(&dir);
    let db = dir.join("state.sqlite");
    let log_path = dir.join("daemon.log");
    let inbox = dir.join("inbox");

    // 工作站投递两份：一份 GBK（老设备），一份混编（模板与所见不同编码）。
    let body = "病历号：PID-EXAMPLE\n超声所见\n左肾积水 3.2cm\n";
    let gbk_dir = inbox.join("S-DAEMON-GBK");
    std::fs::create_dir_all(&gbk_dir).unwrap();
    std::fs::write(gbk_dir.join("report.txt"), gbk_bytes(body)).unwrap();
    let mixed_dir = inbox.join("S-DAEMON-MIXED");
    std::fs::create_dir_all(&mixed_dir).unwrap();
    let mut mixed = gbk_bytes(body);
    mixed.extend_from_slice("积水明显".as_bytes());
    std::fs::write(mixed_dir.join("report.txt"), mixed).unwrap();

    let mut child = spawn_daemon(&config_path, &log_path);
    let pid = child.id();

    // 等守护进程把两份都处理完（真实落盘状态库）。
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut done = false;
    while Instant::now() < deadline {
        if item_rows(&db) == 1 && failure_count(&db, "ENCODING_MIXED") == 1 {
            done = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(done, "守护进程应处理完两份：台账 1 行 + 混编失败条目 1 条");

    // 优雅退出。
    Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("应能发出 SIGTERM");
    let status = wait_exit(&mut child, Duration::from_secs(20)).expect("应退出");
    assert_eq!(status.code(), Some(0), "SIGTERM 后应优雅退出");

    // 台账：只有 GBK 那份产出条目，归一后的值正确。
    let repo = Repo::open(&db).unwrap();
    // items 表以 report_id 外键挂到 reports —— 检查号在 reports 上，取值要连表。
    let (study_no, value_si, unit_code): (String, f64, String) = repo
        .connection()
        .query_row(
            "SELECT r.study_no, i.value_si, i.unit_code \
             FROM items i JOIN reports r ON r.id = i.report_id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(study_no, "S-DAEMON-GBK");
    assert_eq!(value_si, 32.0, "GBK 报告的 3.2cm 应归一成 32mm");
    assert_eq!(unit_code, "mm");
    assert_eq!(item_rows(&db), 1, "混编报告不得产出条目");

    // 混编那份：失败清单里有它，且带稳定错误码。
    assert_eq!(failure_count(&db, "ENCODING_MIXED"), 1);
    assert_eq!(failure_count(&db, "ENCODING_UNKNOWN"), 0);

    std::fs::remove_dir_all(&dir).ok();
}
