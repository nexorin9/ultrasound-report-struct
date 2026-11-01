//! Task 41 的集成验收：**回写失败退避重试 + `pending_writeback` 告警**。
//!
//! 这一层的全部主张是「**失败不静默改数据**」，四组断言：
//!
//! 1. **重试有上限** —— EMR 持续 500 → 试满 3 次（首次 + 2 次退避重试）就停，
//!    状态如实落 `pending_writeback`，**台账行一行都不丢**；
//! 2. **告警真的发出去** —— 落 `pending_writeback` 的那一刻发企微告警，
//!    正文含**检查号 + 稳定错误码 + 已尝试次数**，**不含报告原文**；
//! 3. **恢复路径** —— `pending_writeback` 的报告可由 replay 通道补处理，
//!    重试成功 → 回到 `written_back`；
//! 4. **幂等不重复写** —— 补处理成功后**再补一次不会发第二次写请求**
//!    （幂等键挡住了）；写失败时**把钥匙让出来**，下一轮补处理登记得上。
//!
//! **替身不是最终对接边界**：这里起的是 `127.0.0.1` 上的裸 HTTP 服务，
//! 用来断言**客户端真的按契约发了什么、失败后真的按策略退避了**。
//! 院内真实对接边界是 `spec.md` §2.3 那两行
//! （`POST {EMR_BASE_URL}/api/v1/structured-reports` 与 `POST {WECOM_WEBHOOK_URL}`），
//! 凭据只从环境变量读；真实 EMR / 企微尚未实连（凭据不在本环境），见 `progress.txt`。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ultrasound_report_struct::outbound::emr::EmrClient;
use ultrasound_report_struct::outbound::retry::{
    RetryError, RetryPolicy, WritebackChannel,
};
use ultrasound_report_struct::outbound::review_api::{ReviewHandler, ReviewRequest};
use ultrasound_report_struct::outbound::wecom::WeComClient;
use ultrasound_report_struct::parse::parse_report;
use ultrasound_report_struct::state::idempotency_key;
use ultrasound_report_struct::state::repo::{IngestRecord, NewComparison, Repo};
use ultrasound_report_struct::workflow::audit::AuditRecorder;

/// 端点令牌（测试用；真实环境从 `DAEMON_API_TOKEN` 读）。
const TOKEN: &str = "retry-token-example";
/// EMR 凭据（测试替身用）。
const EMR_TOKEN: &str = "emr-token-example";
/// 确认人账号。
const REVIEWER: &str = "USR-SONO-EXAMPLE";
/// 一份脱敏泌尿系报告（**叙述段是不允许外发的东西**）。
const REPORT: &str = "超声所见：\n左肾前后径 3.2cm\n肾盂轻度扩张\n提示：左肾积水，请结合临床。";
/// 报告里的叙述片段（断言告警与推送正文里**一个字都不许出现**）。
const RAW_FRAGMENTS: [&str; 4] = ["超声所见", "肾盂轻度扩张", "请结合临床", "左肾积水"];

/// 替身收到的一次请求（**含请求体**）。
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 企微推送正文（`text.content`）。
    fn content(&self) -> String {
        let value: serde_json::Value =
            serde_json::from_str(&self.body).expect("推送体应是合法 JSON");
        value["text"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

/// 本地 HTTP 替身（**只在测试数据层**）：同时扮演 EMR 与企业微信群机器人。
///
/// EMR 那一端的状态码**可运行时切换** —— 「先一直失败，过会儿恢复」
/// 这条补处理路径必须能真跑出来。
struct Stub {
    base_url: String,
    emr_status: Arc<AtomicU16>,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Stub {
    /// 起替身。`emr_status` 是 EMR 那一端的应答状态码（200 = 写成功）。
    fn start(emr_status: u16) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定替身端口");
        let addr = listener.local_addr().expect("取替身地址");
        listener.set_nonblocking(true).expect("替身转非阻塞");

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let emr_status = Arc::new(AtomicU16::new(emr_status));
        let thread_recorded = Arc::clone(&recorded);
        let thread_stop = Arc::clone(&stop);
        let thread_status = Arc::clone(&emr_status);

        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let rec = Arc::clone(&thread_recorded);
                        let status = Arc::clone(&thread_status);
                        thread::spawn(move || serve(stream, status, rec));
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Stub {
            base_url: format!("http://{}", addr),
            emr_status,
            recorded,
            stop,
            handle: Some(handle),
        }
    }

    /// 切换 EMR 那一端的状态码（「上游恢复了」）。
    fn set_emr_status(&self, status: u16) {
        self.emr_status.store(status, Ordering::SeqCst);
    }

    /// 收到的 EMR 写请求（`POST /api/v1/structured-reports`）。
    fn emr_writes(&self) -> Vec<Recorded> {
        self.recorded
            .lock()
            .expect("取替身记录")
            .iter()
            .filter(|r| r.target.starts_with("/api/v1/structured-reports"))
            .cloned()
            .collect()
    }

    /// 收到的企微推送（`POST /cgi-bin/webhook/send`）。
    fn wecom_pushes(&self) -> Vec<Recorded> {
        self.recorded
            .lock()
            .expect("取替身记录")
            .iter()
            .filter(|r| r.target.contains("/cgi-bin/webhook/send"))
            .cloned()
            .collect()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 替身的一次应答。
fn serve(mut stream: TcpStream, emr_status: Arc<AtomicU16>, recorded: Arc<Mutex<Vec<Recorded>>>) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut content_length = 0usize;
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((key, value));
        }
    }
    let mut body_bytes = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body_bytes);
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    recorded.lock().expect("记替身请求").push(Recorded {
        method,
        target: target.clone(),
        headers,
        body: String::from_utf8_lossy(&body_bytes).to_string(),
    });

    let (status, body) = if target.starts_with("/api/v1/structured-reports") {
        let status = emr_status.load(Ordering::SeqCst);
        (
            status,
            if status == 200 {
                "{\"id\":\"SR-EXAMPLE\",\"status\":\"accepted\"}".to_string()
            } else {
                "{\"error\":\"EMR 侧出错\"}".to_string()
            },
        )
    } else if target.contains("/cgi-bin/webhook/send") {
        (200, "{\"errcode\":0,\"errmsg\":\"ok\"}".to_string())
    } else {
        (404, "{\"error\":\"unknown path\"}".to_string())
    };
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        reason,
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// 测试用临时目录。
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("urs-retry-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 一份已落台账的报告（**含一条同侧历史差值**）。
struct Fixture {
    db: PathBuf,
    study_no: String,
}

impl Fixture {
    fn repo(&self) -> Repo {
        Repo::open(&self.db).expect("打开状态库")
    }

    fn status(&self) -> String {
        self.repo()
            .find_by_study_no(&self.study_no)
            .expect("查报告")
            .expect("报告应在库里")
            .status
    }

    /// 台账行数（**结论不能丢**）。
    fn item_count(&self) -> i64 {
        self.repo()
            .connection()
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap()
    }

    /// 审计链上的动作序列。
    fn actions(&self) -> Vec<String> {
        let repo = self.repo();
        let report = repo.find_by_study_no(&self.study_no).unwrap().unwrap();
        AuditRecorder::new(&repo)
            .chain(report.id)
            .unwrap()
            .into_iter()
            .map(|row| row.action)
            .collect()
    }
}

/// 落一份脱敏报告（解析 → 强转 → 落台账 → 落一条同侧历史差值）。
fn seed(tag: &str, study_no: &str) -> Fixture {
    let dir = temp_dir(tag);
    let db = dir.join("state.sqlite");
    let repo = Repo::open(&db).unwrap();
    let outcome = parse_report(REPORT);
    assert_eq!(outcome.items.len(), 1, "这份报告应解析出一条测值");
    let result = repo
        .commit_ingestion(&IngestRecord {
            study_no,
            patient_id: Some("PID-EXAMPLE"),
            source_path: None,
            idempotency_key: &idempotency_key(study_no, REPORT),
            dictionary_version: ultrasound_report_struct::dictionary_version(),
            items: &outcome.items,
            failures: &outcome.failures,
        })
        .unwrap();
    assert_eq!(result.status, "parsed");
    repo.insert_comparison(&NewComparison {
        report_id: result.report_id,
        item_id: None,
        part_key: Some("kidney.left".to_string()),
        item_code: Some("kidney.ap_diameter".to_string()),
        previous_value: Some(28.0),
        current_value: Some(32.0),
        delta: Some(4.0),
        direction: Some("增大".to_string()),
        unit_code: Some("mm".to_string()),
        state: "has_history".to_string(),
    })
    .unwrap();
    Fixture {
        db,
        study_no: study_no.to_string(),
    }
}

/// 处理器（出站指向本地替身；退避间隔取 0 —— 测试不等）。
fn handler(fixture: &Fixture, stub: &Stub) -> ReviewHandler {
    let emr = EmrClient::new(&stub.base_url, EMR_TOKEN, Duration::from_secs(3)).unwrap();
    let wecom = WeComClient::new(
        &format!("{}/cgi-bin/webhook/send?key=DEMO-KEY-NOT-REAL", stub.base_url),
        Duration::from_secs(3),
    )
    .unwrap();
    ReviewHandler::with_policy(
        fixture.db.clone(),
        TOKEN.to_string(),
        Some(emr),
        Some(wecom),
        RetryPolicy::immediate(),
    )
}

/// 出站客户端（指向本地替身；退避间隔取 0 —— 测试不等）。
fn clients(stub: &Stub) -> (EmrClient, WeComClient) {
    let emr = EmrClient::new(&stub.base_url, EMR_TOKEN, Duration::from_secs(3)).unwrap();
    let wecom = WeComClient::new(
        &format!("{}/cgi-bin/webhook/send?key=DEMO-KEY-NOT-REAL", stub.base_url),
        Duration::from_secs(3),
    )
    .unwrap();
    (emr, wecom)
}

/// 确认回执体。
fn confirm_body() -> String {
    serde_json::json!({
        "decision": "confirm",
        "reviewer": REVIEWER,
        "note": "核对无误"
    })
    .to_string()
}

#[test]
fn 写失败两次后转待人工介入并有企微告警() {
    let fixture = seed("exhaust", "S-RETRY-EXHAUST");
    let stub = Stub::start(500);
    let handler = handler(&fixture, &stub);

    let response = handler.handle(&ReviewRequest::new(
        "POST",
        &format!("/api/v1/reports/{}/review", fixture.study_no),
        Some(TOKEN),
        &confirm_body(),
    ));
    assert_eq!(response.status, 502, "响应体：{}", response.body);

    // **退避重试有上限**：试满 3 次（首次 + 2 次退避重试）就停。
    assert_eq!(
        stub.emr_writes().len(),
        3,
        "EMR 写请求应是 3 次（首次 + 2 次重试）"
    );
    // 每次写请求都带契约要求的头（幂等键逐次相同 —— 同一把钥匙）。
    let writes = stub.emr_writes();
    let keys: Vec<String> = writes
        .iter()
        .map(|r| {
            r.header("Idempotency-Key")
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(keys.len(), 3);
    assert!(keys.iter().all(|k| k.len() == 64), "幂等键应是 64 位十六进制");
    assert!(
        keys.iter().all(|k| k == &keys[0]),
        "三次尝试用的是同一把幂等键：{:?}",
        keys
    );

    // **状态如实**：落 `pending_writeback`（等重试 / 人工介入），不是「已回写」。
    assert_eq!(fixture.status(), "pending_writeback");
    // **台账行不丢** —— 结论没丢，丢的是回写这一步。
    assert_eq!(fixture.item_count(), 1, "台账行不该丢");

    // **告警真的发出去**：企微收到一条，内容含检查号 + 稳定错误码 + 已尝试次数。
    let pushes = stub.wecom_pushes();
    assert_eq!(pushes.len(), 1, "应有且只有一条告警");
    let content = pushes[0].content();
    assert!(content.contains("S-RETRY-EXHAUST"), "告警应含检查号：{}", content);
    assert!(
        content.contains("EMR_UPSTREAM_UNAVAILABLE"),
        "告警应含稳定错误码：{}",
        content
    );
    assert!(
        content.contains("已尝试：3 次"),
        "告警应含已尝试次数：{}",
        content
    );
    assert!(
        content.contains("待人工介入"),
        "告警应说清是待人工介入：{}",
        content
    );
    // **告警不含报告原文** —— 叙述片段一个字都不许出现。
    for fragment in RAW_FRAGMENTS {
        assert!(
            !content.contains(fragment),
            "告警里混进了报告原文「{}」：{}",
            fragment,
            content
        );
    }

    // **留痕**：失败条目带 EMR 稳定码，告警推送也留痕。
    let actions = fixture.actions();
    assert!(actions.contains(&"failed".to_string()), "审计链：{:?}", actions);
    assert!(actions.contains(&"pushed".to_string()), "审计链：{:?}", actions);
    let repo = fixture.repo();
    let report = repo.find_by_study_no(&fixture.study_no).unwrap().unwrap();
    let chain = AuditRecorder::new(&repo).chain(report.id).unwrap();
    let failure = chain
        .iter()
        .find(|row| row.action == "failed")
        .expect("应有失败留痕");
    assert_eq!(
        failure.error_code.as_deref(),
        Some("EMR_UPSTREAM_UNAVAILABLE")
    );
}

#[test]
fn 不可重试的失败只试一次并直接转待人工介入() {
    let fixture = seed("authfail", "S-RETRY-AUTHFAIL");
    let stub = Stub::start(401);
    let handler = handler(&fixture, &stub);

    let response = handler.handle(&ReviewRequest::new(
        "POST",
        &format!("/api/v1/reports/{}/review", fixture.study_no),
        Some(TOKEN),
        &confirm_body(),
    ));
    assert_eq!(response.status, 502, "响应体：{}", response.body);

    // **鉴权失败重试无用** —— 只试一次，不把一次失败变成三次失败。
    assert_eq!(stub.emr_writes().len(), 1, "不可重试的失败只应试一次");
    // 状态照样如实落 `pending_writeback`（等人换令牌）。
    assert_eq!(fixture.status(), "pending_writeback");
    assert_eq!(fixture.item_count(), 1, "台账行不该丢");
    // 告警带的是鉴权失败那个码。
    let pushes = stub.wecom_pushes();
    assert_eq!(pushes.len(), 1);
    let content = pushes[0].content();
    assert!(content.contains("EMR_AUTH_FAILED"), "告警：{}", content);
    assert!(content.contains("已尝试：1 次"), "告警：{}", content);
}

#[test]
fn 补处理成功回到已回写且台账行不丢() {
    let fixture = seed("recover", "S-RETRY-RECOVER");
    // 模拟「医师确认过、EMR 那时写不进去」：状态停在 `pending_writeback`。
    {
        let repo = fixture.repo();
        let report = repo.find_by_study_no(&fixture.study_no).unwrap().unwrap();
        repo.set_report_status(report.id, "pending_writeback")
            .unwrap();
    }
    let stub = Stub::start(200);
    let repo = fixture.repo();
    let (emr, wecom) = clients(&stub);
    let channel = WritebackChannel::new(&repo, Some(&emr), Some(&wecom), RetryPolicy::immediate());
    let result = channel
        .retry_pending(&fixture.study_no)
        .expect("补处理应跑通");

    assert!(result.written, "补处理应写成功：{:?}", result);
    assert_eq!(result.attempts, 1);
    assert!(result.error.is_none());
    // **恢复路径**：回到 `written_back`。
    assert_eq!(fixture.status(), "written_back");
    // **台账行不丢**。
    assert_eq!(fixture.item_count(), 1, "补处理不该动台账行");
    // 补处理真的发了一次写请求，且报文符合契约。
    let writes = stub.emr_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, "POST");
    assert!(writes[0].header("Idempotency-Key").is_some());
    // 完成通知也发了（这次是「回写完成」不是告警）。
    let pushes = stub.wecom_pushes();
    assert_eq!(pushes.len(), 1);
    let content = pushes[0].content();
    assert!(content.contains("已确认回写"), "推送：{}", content);
    assert!(!content.contains("待人工介入"), "推送：{}", content);
    // 审计链上有「已回写」。
    let actions = fixture.actions();
    assert!(actions.contains(&"written_back".to_string()), "审计链：{:?}", actions);
}

#[test]
fn 补处理成功后不重复写() {
    let fixture = seed("nodup", "S-RETRY-NODUP");
    {
        let repo = fixture.repo();
        let report = repo.find_by_study_no(&fixture.study_no).unwrap().unwrap();
        repo.set_report_status(report.id, "pending_writeback")
            .unwrap();
    }
    let stub = Stub::start(200);
    let repo = fixture.repo();
    let (emr, wecom) = clients(&stub);
    let channel = WritebackChannel::new(&repo, Some(&emr), Some(&wecom), RetryPolicy::immediate());

    let first = channel
        .retry_pending(&fixture.study_no)
        .expect("第一次补处理应跑通");
    assert!(first.written);
    assert_eq!(stub.emr_writes().len(), 1);

    // **幂等键保证不重复写**：再补一次 → 显式拒绝，**一条写请求都不许多发**。
    let second = channel.retry_pending(&fixture.study_no);
    assert!(
        second.is_err(),
        "第二次补处理应被拒：{:?}",
        second.map(|r| r.message())
    );
    assert_eq!(
        stub.emr_writes().len(),
        1,
        "不得产生第二次写请求"
    );
}

#[test]
fn 回写幂等键已登记时补处理被拦下() {
    let fixture = seed("claimed", "S-RETRY-CLAIMED");
    {
        let repo = fixture.repo();
        let report = repo.find_by_study_no(&fixture.study_no).unwrap().unwrap();
        repo.set_report_status(report.id, "pending_writeback")
            .unwrap();
        // **事先登记过这把钥匙**（= 这份报告已经写过一次）。
        assert!(repo
            .claim_writeback(&report.idempotency_key, &report.study_no)
            .unwrap());
    }
    let stub = Stub::start(200);
    let repo = fixture.repo();
    let (emr, wecom) = clients(&stub);
    let channel = WritebackChannel::new(&repo, Some(&emr), Some(&wecom), RetryPolicy::immediate());

    let result = channel.retry_pending(&fixture.study_no);
    match result {
        Err(RetryError::AlreadyClaimed(_)) => {}
        other => panic!("已登记的幂等键应拦下补处理：{:?}", other),
    }
    assert_eq!(stub.emr_writes().len(), 0, "一条写请求都不许发");
    assert_eq!(fixture.status(), "pending_writeback", "状态不该被改动");
}

#[test]
fn 补处理写失败仍留待人工介入且把钥匙让出来() {
    let fixture = seed("stillfail", "S-RETRY-STILLFAIL");
    {
        let repo = fixture.repo();
        let report = repo.find_by_study_no(&fixture.study_no).unwrap().unwrap();
        repo.set_report_status(report.id, "pending_writeback")
            .unwrap();
    }
    let stub = Stub::start(500);
    let repo = fixture.repo();
    let (emr, wecom) = clients(&stub);
    let channel = WritebackChannel::new(&repo, Some(&emr), Some(&wecom), RetryPolicy::immediate());

    let result = channel
        .retry_pending(&fixture.study_no)
        .expect("补处理应跑完（失败也是结果）");
    assert!(!result.written, "EMR 持续 500 时补处理应失败");
    assert_eq!(result.attempts, 3, "退避重试应试满 3 次");
    assert_eq!(result.error_code(), Some("EMR_UPSTREAM_UNAVAILABLE"));
    assert!(result.escalated, "失败应升级为待人工介入");
    // 状态**仍**是 `pending_writeback`（没有被写坏，也没被当成已回写）。
    assert_eq!(fixture.status(), "pending_writeback");
    // 台账行不丢。
    assert_eq!(fixture.item_count(), 1);
    // 告警发了（这次带「已尝试：3 次」）。
    let pushes = stub.wecom_pushes();
    assert_eq!(pushes.len(), 1);
    assert!(pushes[0].content().contains("已尝试：3 次"));

    // **钥匙被让出来了** —— 上游恢复后还能再补一次。
    stub.set_emr_status(200);
    let again = channel
        .retry_pending(&fixture.study_no)
        .expect("上游恢复后应能再补一次");
    assert!(again.written, "上游恢复后补处理应成功");
    assert_eq!(fixture.status(), "written_back");
    assert_eq!(fixture.item_count(), 1, "台账行仍不丢");
}

#[test]
fn 补处理只处理待重试的报告() {
    let fixture = seed("notpending", "S-RETRY-NOTPENDING");
    let stub = Stub::start(200);
    let repo = fixture.repo();
    let (emr, wecom) = clients(&stub);
    let channel = WritebackChannel::new(&repo, Some(&emr), Some(&wecom), RetryPolicy::immediate());
    // 报告还在 `parsed`（没人确认过）—— 不该被补处理。
    let result = channel.retry_pending(&fixture.study_no);
    match result {
        Err(RetryError::NotPending { status, .. }) => {
            assert_eq!(status, "parsed");
        }
        other => panic!("非待重试的报告应被拒：{:?}", other),
    }
    assert_eq!(
        stub.emr_writes().len(),
        0,
        "不该为没确认过的报告发写请求"
    );
}

#[test]
fn 退避次数与间隔从配置来() {
    use ultrasound_report_struct::config::Config;
    let dir = temp_dir("policy");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "[outbound]\nwriteback_max_retries = 1\nwriteback_backoff_ms = 50\n",
    )
    .unwrap();
    let config = Config::load(&path).unwrap();
    let policy = RetryPolicy::from_config(&config);
    assert_eq!(policy.max_retries, 1);
    assert_eq!(policy.max_attempts(), 2);
    assert_eq!(policy.backoff_ms(1), 50);

    // 默认值：重试 2 次、基数 200ms。
    let default = RetryPolicy::from_config(&Config::default());
    assert_eq!(default.max_retries, 2);
    assert_eq!(default.base_backoff_ms, 200);
    assert_eq!(default.max_attempts(), 3);
}
