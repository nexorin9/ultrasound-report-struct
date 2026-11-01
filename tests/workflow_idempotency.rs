//! 工作流第 3 层验收：幂等键与事务边界（重复投递不产生第二条台账行）。
//!
//! **验收的是三件在院内真会同时发生的事**：
//!
//! 1. **重放** —— 工作站重发 / 守护进程重启后续跑：同一份报告被送进来 3 次，
//!    台账里只能有 1 行，**受理的写请求也只能是 1 次**（另 2 次被幂等拦下并留痕）；
//! 2. **并发重投** —— 定时任务与守护进程撞上同一份报告：
//!    两条连接同时写同一检查号，**只落一条台账行**（数据库唯一约束兜底，
//!    不靠进程内内存锁 —— 进程重启后依然成立）；
//! 3. **失败原子性** —— 一份报告里混进一条**不合法**的台账行：
//!    整笔回滚，**不落半条台账**（报告行 / 条目 / 审计 / 幂等登记全部撤销）。
//!
//! 源产品同构证据：`github_ref/pgrust/crates/_support/mcx/src/aset.rs` +
//! `tests.rs` —— 分配与释放走**同一个决策**（`is_dedicated` 是纯函数），
//! 且**失败的申请不扣账**（`failed reservation charged nothing`）。
//! 本项目改写为「幂等查重与写库走同一把钥匙」+「事务失败不落半条台账」。

use ultrasound_report_struct::ingest::ingest_inbox;
use ultrasound_report_struct::parse::{parse_report, ParsedItem};
use ultrasound_report_struct::state::repo::{IngestRecord, Repo, RepoError};
use ultrasound_report_struct::state::idempotency_key;
use ultrasound_report_struct::workflow::idempotency::{
    IdempotencyGuard, IdempotencyOutcome, SCOPE_INGEST,
};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

/// 一份脱敏报告（占位检查号 / 占位患者主索引，不含任何真实标识）。
const BODY: &str = "病历号：PID-EXAMPLE\n超声所见：\n左肾前后径 3.2cm";

/// 临时目录（每个用例一个，跑完清掉）。
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "urs-idem-{}-{}-{:?}",
        tag,
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 开一个状态库（落到磁盘 —— 并发用例要多个连接连同一个库）。
fn open_db(dir: &Path, name: &str) -> Repo {
    Repo::open(&dir.join(name)).unwrap()
}

/// 造一条入库记录。
fn record<'a>(study_no: &'a str, key: &'a str, items: &'a [ParsedItem]) -> IngestRecord<'a> {
    IngestRecord {
        study_no,
        patient_id: Some("PID-EXAMPLE"),
        source_path: Some("inbox/SAMPLE-A/report.txt"),
        idempotency_key: key,
        dictionary_version: ultrasound_report_struct::dictionary_version(),
        items,
        failures: &[],
    }
}

/// 表行数。
fn count(repo: &Repo, table: &str) -> i64 {
    repo.connection()
        .query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// **重放三次：台账行数与写请求数均不变**。
///
/// 「写请求数」的口径 = 幂等登记表里 `ingest` 用途的行数：
/// 每一笔**真正落库**的写入登记一行，被幂等拦下的**不登记**
/// （登记在事务里，随回滚一起撤销 —— 失败的写入不扣账）。
#[test]
fn 重放三次台账行数与写请求数均不变() {
    let dir = temp_dir("replay");
    let repo = open_db(&dir, "state.db");
    let guard = IdempotencyGuard::new(&repo);
    let outcome = parse_report(BODY);
    let key = idempotency_key("SAMPLE-REPLAY", BODY);
    let rec = record("SAMPLE-REPLAY", &key, &outcome.items);

    let mut committed = 0;
    let mut skipped = 0;
    for _ in 0..3 {
        match guard.commit_ingestion(&rec).unwrap() {
            IdempotencyOutcome::Committed { .. } => committed += 1,
            IdempotencyOutcome::Duplicate { .. } => skipped += 1,
        }
    }

    assert_eq!(committed, 1, "三次重放只受理一次");
    assert_eq!(skipped, 2, "另外两次被幂等拦下");
    assert_eq!(count(&repo, "items"), 1, "台账行数不变");
    assert_eq!(count(&repo, "reports"), 1, "报告行数不变");
    assert_eq!(
        guard.accepted_write_requests(SCOPE_INGEST).unwrap(),
        1,
        "写请求数不变"
    );
    assert_eq!(guard.skipped_write_requests().unwrap(), 2);
    // 重复投递不是「又收了一份」—— 接收留痕只应有一条。
    let received: i64 = repo
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'received'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(received, 1, "接收留痕只应有一条");
}

/// **幂等命中要留痕**：否则运维看不出「少的行去哪了」。
#[test]
fn 幂等命中留痕指向被跳过的那份内容() {
    let dir = temp_dir("audit");
    let repo = open_db(&dir, "state.db");
    let guard = IdempotencyGuard::new(&repo);
    let outcome = parse_report(BODY);
    let key = idempotency_key("SAMPLE-AUDIT", BODY);
    let rec = record("SAMPLE-AUDIT", &key, &outcome.items);

    guard.commit_ingestion(&rec).unwrap();
    let dup = guard.commit_ingestion(&rec).unwrap();
    assert!(dup.is_duplicate());
    // 跳过后**沿用已有那份报告**的主键 —— 不新建报告。
    assert_eq!(dup.report_id(), guard.commit_ingestion(&rec).unwrap().report_id());

    let (action, digest, detail, study): (String, String, String, String) = repo
        .connection()
        .query_row(
            "SELECT action, input_digest, detail, study_no FROM audit_log \
             WHERE action = 'duplicate_skipped'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(action, "duplicate_skipped");
    assert_eq!(digest, key, "留痕摘要与幂等键同源");
    assert_eq!(study, "SAMPLE-AUDIT");
    assert!(detail.contains("不产生第二条台账行"), "留痕要说得清去向");
}

/// **并发同 study_no 只落一条台账行**（唯一约束兜底，不靠进程内内存锁）。
///
/// 8 条连接（各自 `Repo`）同时提交同一份报告：
/// 一条真正落库，其余被判幂等命中 —— 台账行**只有 1 条**。
#[test]
fn 并发同检查号只落一条台账行() {
    let dir = temp_dir("concurrent");
    let db_path = dir.join("state.db");
    let outcome = parse_report(BODY);
    let key = idempotency_key("SAMPLE-CONC", BODY);

    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut handles = Vec::new();

    // 每条线程一条连接（守护进程也是每线程一条连接）；连接**先开好**再分给线程 ——
    // 万一开连接失败，线程会 panic 在 barrier 之前，其余线程就永远等在 barrier 上。
    let repos: Vec<Repo> = (0..workers)
        .map(|_| Repo::open(&db_path).unwrap())
        .collect();

    for repo in repos {
        let barrier = Arc::clone(&barrier);
        let outcome = outcome.clone();
        let key = key.clone();
        handles.push(std::thread::spawn(move || {
            let guard = IdempotencyGuard::new(&repo);
            let rec = record("SAMPLE-CONC", &key, &outcome.items);
            // 一起冲 —— 让并发窗口真的打开。
            barrier.wait();
            match guard.commit_ingestion(&rec) {
                Ok(outcome) => Ok(outcome.is_committed()),
                Err(e) => Err(format!("{}", e)),
            }
        }));
    }

    let mut committed = 0;
    for handle in handles {
        match handle.join().unwrap() {
            Ok(true) => committed += 1,
            Ok(false) => {}
            Err(msg) => panic!("并发写入不应报错：{}", msg),
        }
    }
    assert_eq!(committed, 1, "并发重投只应有一条真正落库");

    let repo = open_db(&dir, "state.db");
    assert_eq!(count(&repo, "items"), 1, "只落一条台账行");
    assert_eq!(count(&repo, "reports"), 1, "只落一条报告行");
    assert_eq!(count(&repo, "idempotency"), 1, "只登记一次幂等键");
}

/// **非法条目触发整笔回滚**：不落半条台账。
#[test]
fn 非法条目触发整体回滚台账行数不增() {
    let dir = temp_dir("rollback");
    let repo = open_db(&dir, "state.db");
    let guard = IdempotencyGuard::new(&repo);
    let outcome = parse_report(BODY);

    // 注入一条**不合法**的台账行：有归一值、没单位码。
    let mut items = outcome.items.clone();
    let mut bad = items[0].clone();
    bad.unit_code = None;
    items.push(bad);

    let key = idempotency_key("SAMPLE-BAD", BODY);
    let err = guard
        .commit_ingestion(&record("SAMPLE-BAD", &key, &items))
        .unwrap_err();
    assert!(
        matches!(err, RepoError::InvalidItem(_)),
        "非法条目应判为台账行不合法，实际：{}",
        err
    );

    // **不落半条台账**：报告行、条目、审计、幂等登记全部撤销。
    assert_eq!(count(&repo, "items"), 0, "不得落条目");
    assert_eq!(count(&repo, "reports"), 0, "不得落报告行");
    assert_eq!(count(&repo, "audit_log"), 0, "不得留审计");
    assert_eq!(count(&repo, "idempotency"), 0, "不得登记幂等键");
    assert_eq!(
        guard.accepted_write_requests(SCOPE_INGEST).unwrap(),
        0,
        "失败的写入不扣账"
    );

    // 回滚后这份报告**还能被正常处理**（回滚没有把库写坏）。
    let good = parse_report(BODY);
    let ok = guard
        .commit_ingestion(&record("SAMPLE-BAD", &key, &good.items))
        .unwrap();
    assert!(ok.is_committed(), "回滚后同一份报告应能正常落库");
    assert_eq!(count(&repo, "items"), 1);
}

/// **幂等键 = 审计摘要哈希**（同源、用途分开）。
#[test]
fn 幂等键与审计摘要同源() {
    let dir = temp_dir("digest");
    let repo = open_db(&dir, "state.db");
    let guard = IdempotencyGuard::new(&repo);
    let outcome = parse_report(BODY);
    let key = idempotency_key("SAMPLE-DIGEST", BODY);
    guard
        .commit_ingestion(&record("SAMPLE-DIGEST", &key, &outcome.items))
        .unwrap();

    let digests: Vec<String> = {
        let mut stmt = repo
            .connection()
            .prepare("SELECT input_digest FROM audit_log WHERE action = 'received'")
            .unwrap();
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    };
    assert_eq!(digests.len(), 1);
    assert_eq!(
        digests[0], key,
        "审计摘要与幂等键必须同源（同算法、同拼接顺序）"
    );
}

/// **真实投递批次路径**：重投同一批 → 台账行数不变，且**每一次跳过都留痕**。
///
/// 走的是 `ingest --inbox` 用的那条路径（不是测试专用的构造），
/// 验的是产品真的会走的路。
#[test]
fn 投递批次重投后台账行数不变且每次跳过都留痕() {
    let dir = temp_dir("batch");
    let repo = open_db(&dir, "state.db");
    let inbox = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/samples/inbox");

    let first = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(first.reports, 2, "两份样例报告都应入库");
    assert_eq!(first.items, 1);
    assert_eq!(first.duplicates, 0);

    let second = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(second.reports, 0, "重投不应再入库");
    assert_eq!(second.items, 0);
    assert_eq!(second.duplicates, 2, "两份都应被判为重复投递");

    assert_eq!(count(&repo, "items"), 1, "台账行数不变");
    assert_eq!(count(&repo, "reports"), 2, "报告行数不变");
    // 每一次跳过都要留痕 —— 运维要看得见「这份被跳过了」。
    let skipped: i64 = repo
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'duplicate_skipped'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(skipped, 2, "两次重复投递各留一条 duplicate_skipped");
    // 但**接收留痕不能被重复投递撑多** —— 它不是又收了一份。
    let received: i64 = repo
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'received'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(received, 2, "接收留痕仍是两份报告各一条");
}
