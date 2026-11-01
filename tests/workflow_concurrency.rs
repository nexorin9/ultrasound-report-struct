//! 工作流第 4 层验收：**并发保护**（同检查号串行、不同检查号并行）。
//!
//! **验收的是三件在院内真会同时发生的事**：
//!
//! 1. **工作站重发 / 人工重投** —— 同一份报告从两条路进来：
//!    8 条连接同时提交同一检查号，台账里**只能有 1 行**（不是 8 行）；
//! 2. **定时批处理与守护进程重叠** —— 两个进程撞同一份报告：
//!    串行化必须**跨进程**成立（锁在库里，不在进程内存里）；
//! 3. **同一检查号、内容不同** —— 工作站改过又重导：
//!    幂等键挡不住（内容变了键就变了），**检查号唯一约束**才拦得住 ——
//!    台账里仍然只能有 1 行，且这次跳过**要留痕说得清**（运维要追）。
//!
//! **反向验收**：**不同检查号必须能并行** ——
////! 串行化做成一把全局锁的话，台账虽然对了，吞吐却塌了
//! （日均几十份报告、每份还要去 RIS 取一次历史值）。
//!
//! 测试替身（RIS / EMR / 企微）不在本层范围 —— 这一层只管**并发与串行**。

use ultrasound_report_struct::daemon::processor::{IngestProcessor, ProcessError, ReportProcessor};
use ultrasound_report_struct::ingest::ingest_inbox;
use ultrasound_report_struct::ingest::watcher::{Candidate, CandidateKind};
use ultrasound_report_struct::state::idempotency_key;
use ultrasound_report_struct::state::repo::Repo;
use ultrasound_report_struct::workflow::concurrency::{
    ConcurrencyError, StudyNoGate, DEFAULT_ATTEMPTS, DEFAULT_LEASE_SECS,
};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::Duration;

/// 一份脱敏报告（占位检查号 / 占位患者主索引，不含任何真实标识）。
const BODY_A: &str = "病历号：PID-EXAMPLE\n超声所见：\n左肾前后径 3.2cm";
/// 同一检查号、**内容不同**的另一份（工作站改过又重导的情形）。
const BODY_B: &str = "病历号：PID-EXAMPLE\n超声所见：\n左肾前后径 3.5cm";

/// 临时目录（每个用例一个，跑完清掉）。
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "urs-conc-{}-{}-{:?}",
        tag,
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 开一个状态库（落盘 —— 并发用例要多个连接连同一个库）。
fn open_db(dir: &Path, name: &str) -> Repo {
    Repo::open(&dir.join(name)).unwrap()
}

/// 造一个投递候选（TXT 通道）。
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

/// 表行数。
fn count(repo: &Repo, table: &str) -> i64 {
    repo.connection()
        .query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// **并发同检查号只落一条台账行**。
///
/// 8 条连接（各自 `Repo`，相当于守护进程的 8 个 worker 或两个进程）
/// 同时提交同一份报告：受理 1 次，其余被判重复投递 —— 台账行**只有 1 条**。
#[test]
fn 并发同检查号只落一条台账行() {
    let dir = temp_dir("same-study");
    let db_path = dir.join("state.db");

    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut handles = Vec::new();

    // 连接**先开好**再分给线程：每条线程一条连接（守护进程也是每线程一条），
    // 但开连接放到线程外 —— 万一开连接失败，线程会 panic 在 barrier 之前，
    // 其余线程就**永远等在 barrier 上**（整个测试挂死，而不是干脆地失败）。
    let repos: Vec<Repo> = (0..workers)
        .map(|_| Repo::open(&db_path).unwrap())
        .collect();

    for repo in repos {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let cand = candidate("S-CONC-SAME", BODY_A);
            barrier.wait();
            match IngestProcessor.process(&repo, &cand) {
                Ok(outcome) => Ok(outcome),
                Err(e) => Err(e.to_string()),
            }
        }));
    }

    let mut committed = 0usize;
    let mut duplicates = 0usize;
    for handle in handles {
        match handle.join().unwrap() {
            Ok(outcome) => {
                if outcome.duplicate {
                    duplicates += 1;
                } else {
                    committed += 1;
                }
            }
            Err(msg) => panic!("并发写入不应报错：{}", msg),
        }
    }
    assert_eq!(committed, 1, "并发重投只应有一条真正落库");
    assert_eq!(duplicates, workers - 1, "其余都应被判为重复投递");

    let repo = open_db(&dir, "state.db");
    assert_eq!(count(&repo, "items"), 1, "只落一条台账行");
    assert_eq!(count(&repo, "reports"), 1, "只落一条报告行");
    assert_eq!(count(&repo, "idempotency"), 1, "只登记一次幂等键");
    // 锁用完即还：库里不该留下锁行（否则下次投递会被自己的旧锁挡住）。
    assert_eq!(count(&repo, "study_locks"), 0, "处理完不该留下锁行");
}

/// **同一检查号、内容不同：也只落一条台账行，且这次跳过要留痕说得清**。
///
/// 幂等键挡不住（内容变了键就变了）—— 拦下它的是**检查号唯一约束**。
/// 台账里只能有 1 行；留痕要能分清「同一份内容重发」与「同一检查号、内容不同」。
#[test]
fn 同检查号内容不同也只落一条台账行且留痕说得清() {
    let dir = temp_dir("conflict");
    let db_path = dir.join("state.db");

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    // 连接先开好再分给线程（开连接失败时不至于让另一条线程永远等在 barrier 上）。
    let repos: Vec<Repo> = [BODY_A, BODY_B]
        .iter()
        .map(|_| Repo::open(&db_path).unwrap())
        .collect();
    for (i, (body, repo)) in [BODY_A, BODY_B].into_iter().zip(repos).enumerate() {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let cand = candidate("S-CONC-CONFLICT", body);
            barrier.wait();
            IngestProcessor.process(&repo, &cand)
                .map(|o| (i, o))
                .map_err(|e| e.to_string())
        }));
    }

    let mut committed = 0usize;
    for handle in handles {
        match handle.join().unwrap() {
            Ok((_, outcome)) => {
                if !outcome.duplicate {
                    committed += 1;
                }
            }
            Err(msg) => panic!("并发写入不应报错：{}", msg),
        }
    }
    assert_eq!(committed, 1, "同一检查号只能落一份报告");

    let repo = open_db(&dir, "state.db");
    assert_eq!(count(&repo, "reports"), 1, "只落一条报告行");
    assert_eq!(count(&repo, "items"), 1, "只落一条台账行");

    // 留痕：这次跳过是「检查号冲突」，不是「幂等键命中」——
    // 两者的处置方式不同（后者跳过即可，前者要追）。
    let (detail,): (String,) = repo
        .connection()
        .query_row(
            "SELECT detail FROM audit_log WHERE action = 'duplicate_skipped'",
            [],
            |row| Ok((row.get(0)?,)),
        )
        .unwrap();
    assert!(
        detail.contains("检查号"),
        "留痕要说明是检查号冲突，实际：{}",
        detail
    );
    assert!(
        detail.contains("不产生第二条台账行"),
        "留痕要说得清去向，实际：{}",
        detail
    );
}

/// **不同检查号可以并行**（串行化做成全局锁的话吞吐就塌了）。
///
/// 证明**真的重叠**：两个线程各持一把**不同**检查号的锁，
/// 双方都要等对方进来才继续 —— 要是被串行化了，
/// 先到的那位会**等超时**（而不是永远挂着）。
#[test]
fn 不同检查号可以并行处理() {
    let dir = temp_dir("parallel");
    let db_path = dir.join("state.db");
    let repo = open_db(&dir, "state.db");

    let (tx_a, rx_a) = std::sync::mpsc::channel::<()>();
    let (tx_b, rx_b) = std::sync::mpsc::channel::<()>();

    let mut handles = Vec::new();
    {
        let db_path = db_path.clone();
        handles.push(std::thread::spawn(move || {
            let repo = Repo::open(&db_path).unwrap();
            let gate = StudyNoGate::with_holder(&repo, "holder-A");
            // 各自抢**不同**检查号的锁：都该抢得到。
            let lease = gate.try_acquire("S-PARA-A", DEFAULT_LEASE_SECS).unwrap();
            tx_a.send(()).unwrap();
            // 等对方也持上锁 —— 等不到就说明被串行化了。
            rx_b.recv_timeout(Duration::from_secs(10))
                .expect("对方应能同时持锁（不同检查号不该互相等待）");
            drop(lease);
        }));
    }
    {
        let db_path = db_path.clone();
        handles.push(std::thread::spawn(move || {
            let repo = Repo::open(&db_path).unwrap();
            let gate = StudyNoGate::with_holder(&repo, "holder-B");
            let lease = gate.try_acquire("S-PARA-B", DEFAULT_LEASE_SECS).unwrap();
            tx_b.send(()).unwrap();
            rx_a.recv_timeout(Duration::from_secs(10))
                .expect("对方应能同时持锁（不同检查号不该互相等待）");
            drop(lease);
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    // 各查各的历史、各落各的库：两个检查号都进台账。
    let outcome_a = IngestProcessor
        .process(&repo, &candidate("S-PARA-A", BODY_A))
        .unwrap();
    let outcome_b = IngestProcessor
        .process(&repo, &candidate("S-PARA-B", BODY_A))
        .unwrap();
    assert_eq!(outcome_a.items, 1);
    assert_eq!(outcome_b.items, 1);
    assert_eq!(count(&repo, "items"), 2, "不同检查号各自落库");
    assert_eq!(count(&repo, "reports"), 2);
}

/// **串行化跨进程成立**：锁在库里，不在进程内存里。
///
/// 一条连接抢着锁，另一条连接（相当于另一个进程）就抢不到 ——
///// 换句话说：进程内那份在途集合挡不住的事，这把锁挡得住。
#[test]
fn 串行化跨进程成立() {
    let dir = temp_dir("cross-process");
    let repo = open_db(&dir, "state.db");

    let gate = StudyNoGate::with_holder(&repo, "holder-in-process-A");
    let _lease = gate.try_acquire("S-CROSS", DEFAULT_LEASE_SECS).unwrap();

    // **另一条连接**（另一个进程连同一个库）。
    let other = open_db(&dir, "state.db");
    let other_gate = StudyNoGate::with_holder(&other, "holder-in-process-B");
    match other_gate.try_acquire("S-CROSS", DEFAULT_LEASE_SECS) {
        Err(ConcurrencyError::Held { holder, .. }) => {
            assert_eq!(holder, "holder-in-process-A", "要说得清是谁占着");
        }
        other => panic!("跨进程应抢不到锁，实际：{:?}", other),
    };
}

/// **锁被占着时，处理显式报争用**（不当成处理成功，也不静默丢弃）。
///
/// 这一份交还事件源，下一轮再取 —— 源文件没动，重试是安全的。
#[test]
fn 锁被占着时显式报争用() {
    let dir = temp_dir("contended");
    let repo = open_db(&dir, "state.db");

    // 另一个连接占着这把锁。
    let other = open_db(&dir, "state.db");
    let holder = StudyNoGate::with_holder(&other, "holder-other");
    let _lease = holder.try_acquire("S-CONTENDED", DEFAULT_LEASE_SECS).unwrap();

    let result = IngestProcessor.process(&repo, &candidate("S-CONTENDED", BODY_A));
    match result {
        Err(ProcessError::Concurrency(ConcurrencyError::Contended {
            study_no,
            attempts,
        })) => {
            assert_eq!(study_no, "S-CONTENDED");
            assert_eq!(attempts, DEFAULT_ATTEMPTS);
        }
        other => panic!("应报并发争用，实际：{:?}", other.map(|o| o.duplicate)),
    }

    // 争用**不落台账行**（这一份还没处理，不该出现在台账里）。
    assert_eq!(count(&repo, "items"), 0, "争用不得落台账行");
    assert_eq!(count(&repo, "reports"), 0, "争用不得落报告行");
}

/// **批处理与守护进程重叠跑**：同一份报告不会被处理两遍。
///
/// 运维在 crontab 里加了一趟批量补处理，守护进程也没停 —— 两个进程同跑一个目录。
#[test]
fn 批处理与守护进程重叠跑不重复落库() {
    let dir = temp_dir("overlap");
    let db_path = dir.join("state.db");

    // 自建一个投递目录（放一份脱敏报告）。
    let inbox = dir.join("inbox");
    let report_dir = inbox.join("S-OVERLAP");
    std::fs::create_dir_all(&report_dir).unwrap();
    std::fs::write(report_dir.join("report.txt"), BODY_A).unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();

    // 两条路径各一条连接；连接先开好再分给线程
    // （开连接失败时不至于让另一条线程永远等在 barrier 上）。
    let repo_batch = Repo::open(&db_path).unwrap();
    let repo_daemon = Repo::open(&db_path).unwrap();

    // 一边：批处理路径。
    {
        let inbox = inbox.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            ingest_inbox(&repo_batch, &inbox).map(|s| s.items).map_err(|e| e.to_string())
        }));
    }
    // 另一边：守护进程路径（同一份投递）。
    {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let cand = candidate("S-OVERLAP", BODY_A);
            barrier.wait();
            IngestProcessor.process(&repo_daemon, &cand)
                .map(|o| o.items)
                .map_err(|e| e.to_string())
        }));
    }

    for handle in handles {
        // 两条路径都**不该报错**：抢不到锁的那条要么等到了，要么被判重复投递
        // （真抢不到就是显式报争用，交还事件源下一轮再取）。
        match handle.join().unwrap() {
            Ok(items) => assert!(items <= 1, "一份报告最多一条台账行"),
            Err(msg) => assert!(
                msg.contains("并发") || msg.contains("正在被"),
                "并发处理不应报错：{}",
                msg
            ),
        }
    }

    let repo = open_db(&dir, "state.db");
    assert_eq!(count(&repo, "items"), 1, "重叠跑只落一条台账行");
    assert_eq!(count(&repo, "reports"), 1, "重叠跑只落一条报告行");
    assert_eq!(count(&repo, "study_locks"), 0, "处理完不留锁行");
}

/// **重放不产生第二条台账行**（串行化之后仍然成立）。
#[test]
fn 重放不产生第二条台账行() {
    let dir = temp_dir("replay");
    let repo = open_db(&dir, "state.db");

    for _ in 0..3 {
        let out = IngestProcessor
            .process(&repo, &candidate("S-REPLAY", BODY_A))
            .unwrap();
        assert!(out.duplicate || out.items == 1);
    }
    assert_eq!(count(&repo, "items"), 1, "重放三次台账行仍只有一条");
    assert_eq!(count(&repo, "reports"), 1);
    // 每一次跳过都留痕（运维要看得见「这份被跳过了几次」）。
    let skipped: i64 = repo
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'duplicate_skipped'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(skipped, 2, "两次重复投递各留一条 duplicate_skipped");
    // 锁用完即还：重放不该被自己的旧锁挡住。
    assert_eq!(count(&repo, "study_locks"), 0);
}

/// **并发上限之外的并行度**：8 份**不同**检查号的报告同时处理，各自落库。
///
/// 串行化只作用于**同一检查号**；不同检查号各走各的（并发上限由调度器管）。
#[test]
fn 八个不同检查号并发各自落库() {
    let dir = temp_dir("distinct");
    let db_path = dir.join("state.db");

    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut handles = Vec::new();
    // 连接先开好再分给线程（开连接失败时不至于让其余线程永远等在 barrier 上）。
    let repos: Vec<Repo> = (0..workers)
        .map(|_| Repo::open(&db_path).unwrap())
        .collect();
    for (i, repo) in repos.into_iter().enumerate() {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let study_no = format!("S-DISTINCT-{}", i);
            let cand = candidate(&study_no, BODY_A);
            barrier.wait();
            IngestProcessor.process(&repo, &cand)
                .map(|o| o.items)
                .map_err(|e| e.to_string())
        }));
    }
    for handle in handles {
        assert_eq!(handle.join().unwrap().unwrap(), 1, "各自落一条台账行");
    }

    let repo = open_db(&dir, "state.db");
    assert_eq!(count(&repo, "items"), workers as i64);
    assert_eq!(count(&repo, "reports"), workers as i64);
    assert_eq!(count(&repo, "study_locks"), 0, "处理完不留锁行");
    // 让闸有余量：不同检查号不该互相等待。
    let _ = Duration::from_millis(1);
}
