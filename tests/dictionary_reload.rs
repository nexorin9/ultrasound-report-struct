//! 词典热更新 —— 集成验收（Task 26）。
//!
//! **这一组用例守什么**：
//!
//! 1. **运维改了词典就生效**：守护进程每轮检查词典文件是否变了，
//!    变了就换一代口径 —— **不用停机重启**。
//! 2. **在途任务不换口径**：一份报告从解析到落库用同一代词典；
//!    换代时还在跑的任务继续用旧口径算完（口径留痕说得清）。
//! 3. **口径留痕是真的留**：热更新后**旧台账行仍是旧版本号**，
//!    新台账行是新版本号 —— 「这条数据是哪套口径算出来的」事后查得到。
//!
//! 与单元测试的差别：这里起的是**真的可执行文件**、真的守护进程，
//! 断言的是**真的日志行**与**真的落盘状态库**。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ultrasound_report_struct::state::repo::Repo;

/// 被测可执行文件（cargo 为集成测试注入）。
const BIN: &str = env!("CARGO_BIN_EXE_ultrasound-report-struct");

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "urs-dict-reload-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 写一份词典（版本号可换）+ 配置文件。
///
/// **两本词典都用仓库里真实的那一份起手** —— 词典文件存在却坏了，
/// 守护进程会直接退出（不退回内置旧词典），测试里必须放真词典。
fn write_config(dir: &Path) -> (PathBuf, PathBuf) {
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let dict = dir.join("dict");
    std::fs::create_dir_all(&dict).unwrap();
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::copy(repo_root.join("assets/dictionary/parts.json"), dict.join("parts.json")).unwrap();
    std::fs::copy(repo_root.join("assets/dictionary/items.json"), dict.join("items.json")).unwrap();

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
    (config_path, dict.join("parts.json"))
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

/// 读日志文本。
fn read_log(path: &Path) -> String {
    let mut text = String::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// 台账行的（检查号 → 词典版本）—— 台账行挂在报告下（`items.report_id` → `reports.id`）。
fn ledger_versions(db: &Path) -> Vec<(String, String)> {
    let repo = Repo::open(db).unwrap();
    let mut stmt = repo
        .connection()
        .prepare(
            "SELECT reports.study_no, items.dictionary_version FROM items \
             JOIN reports ON reports.id = items.report_id ORDER BY items.id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// 投递一份报告。
fn deliver(inbox: &Path, study_no: &str) {
    let dir = inbox.join(study_no);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("report.txt"),
        "病历号：PID-EXAMPLE\n超声所见\n左肾积水 3.2cm\n",
    )
    .unwrap();
}

/// 等条件成立（带超时）。
fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout: Duration, what: &str) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = what;
    false
}

#[test]
fn 守护进程发现词典变了自动换一代口径且旧行留旧版本() {
    let dir = temp_dir("daemon");
    let (config_path, parts_file) = write_config(&dir);
    let db = dir.join("state.sqlite");
    let log_path = dir.join("daemon.log");

    let mut child = spawn_daemon(&config_path, &log_path);

    // 1) 第一代口径下投递一份报告 → 落台账行（旧版本号）。
    deliver(&dir.join("inbox"), "S-RELOAD-001");
    assert!(
        wait_until(
            || ledger_versions(&db).len() == 1,
            Duration::from_secs(30),
            "首份报告应被处理"
        ),
        "首份报告应被处理并落台账行"
    );
    let first = ledger_versions(&db);
    assert_eq!(first.len(), 1);
    assert!(
        first[0].1.starts_with("parts-0.2.0+"),
        "首行应带第一代词典版本：{:?}",
        first
    );

    // 2) 运维改词典（版本号 + 多收一个别名 → 内容与长度都变了）。
    let text = std::fs::read_to_string(&parts_file).unwrap();
    let patched = text.replace("\"version\": \"parts-0.2.0\"", "\"version\": \"parts-9.9.9\"");
    assert!(
        patched.contains("parts-9.9.9"),
        "词典版本号应被改掉：{}",
        patched
    );
    std::fs::write(&parts_file, patched).unwrap();

    // 3) 守护进程自己发现词典变了 → 换一代口径（不用停机重启）。
    assert!(
        wait_until(
            || read_log(&log_path).contains("DICT_RELOADED"),
            Duration::from_secs(30),
            "应打出 DICT_RELOADED"
        ),
        "守护进程应自动发现词典变化并换口径：\n{}",
        read_log(&log_path)
    );

    // 4) 换代之后再投递一份 → 新台账行带**新**版本号。
    deliver(&dir.join("inbox"), "S-RELOAD-002");
    assert!(
        wait_until(
            || ledger_versions(&db).len() == 2,
            Duration::from_secs(30),
            "第二份报告应被处理"
        ),
        "第二份报告应被处理并落台账行"
    );
    let both = ledger_versions(&db);
    assert_eq!(both.len(), 2);
    assert!(
        both[0].1.starts_with("parts-0.2.0+"),
        "旧台账行应仍留旧版本号：{:?}",
        both
    );
    assert!(
        both[1].1.starts_with("parts-9.9.9+"),
        "新台账行应带新版本号：{:?}",
        both
    );

    // 5) 停机（真信号），优雅退出。
    Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("应能发出 SIGTERM");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut status = None;
    while Instant::now() < deadline {
        if let Ok(Some(s)) = child.try_wait() {
            status = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(0),
        "SIGTERM 后应优雅退出（退出码 0）"
    );
    let _ = child.wait();

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 手动重载词典换一代口径并报出前后版本() {
    let dir = temp_dir("cli");
    let (config_path, _parts) = write_config(&dir);

    let out = Command::new(BIN)
        .args(["--config", config_path.to_str().unwrap()])
        .args(["reload-dictionary"])
        .output()
        .expect("进程应能起");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "重载应成功退出：\n{}\n{}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("词典已换代"),
        "应报出换代结果：\n{}",
        stdout
    );
    assert!(
        stdout.contains("parts-0.2.0+items-0.1.0"),
        "应报出合并版本号：\n{}",
        stdout
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 词典文件坏了守护进程显式报错且不换口径() {
    // 词典坏了 → **不换口径**（继续用旧的一代），但要显式记事件 ——
    // 值班的人得知道运维改坏了词典，而不是看见台账静悄悄换了口径。
    let dir = temp_dir("broken");
    let (config_path, parts_file) = write_config(&dir);
    let db = dir.join("state.sqlite");
    let log_path = dir.join("daemon.log");

    let mut child = spawn_daemon(&config_path, &log_path);

    // 先跑一份报告，确认第一代口径正常落库。
    deliver(&dir.join("inbox"), "S-BROKEN-001");
    assert!(
        wait_until(
            || ledger_versions(&db).len() == 1,
            Duration::from_secs(30),
            "首份报告应被处理"
        ),
        "首份报告应被处理并落台账行"
    );

    // 把词典改坏。
    std::fs::write(&parts_file, "{\"version\":\"parts-9.9.9\",\"bases\":[],\"entries\":[]}").unwrap();

    // 守护进程应打出显式失败事件，**且不换口径**。
    assert!(
        wait_until(
            || read_log(&log_path).contains("DICT_RELOAD_FAILED"),
            Duration::from_secs(30),
            "应打出 DICT_RELOAD_FAILED"
        ),
        "词典坏了应显式记事件：\n{}",
        read_log(&log_path)
    );
    assert!(
        !read_log(&log_path).contains("DICT_RELOADED"),
        "词典坏了不该换口径：\n{}",
        read_log(&log_path)
    );

    // 停机。
    Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("应能发出 SIGTERM");
    let _ = child.wait();

    // 台账行仍是第一代口径（旧版本号没被换掉）。
    let rows = ledger_versions(&db);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].1.starts_with("parts-0.2.0+"),
        "词典坏了不该改已有行的口径：{:?}",
        rows
    );

    let _ = std::fs::remove_dir_all(&dir);
}
