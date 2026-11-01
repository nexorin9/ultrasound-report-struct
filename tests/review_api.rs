//! Task 40 的集成验收：**内置确认回执端点**（人工确认点，不自动回写）。
//!
//! 端点是本产品**唯一**的人工写入口，也是「不自动回写」这条不变量的落点。
//! 四组断言：
//!
//! 1. **鉴权** —— 缺 `Authorization` 头与令牌不对，一律 401，且**一条写请求都不发**；
//! 2. **不确认就不写** —— 报告落台账、对拍差值算完，**没收到 confirm 就没有 EMR 写请求**；
//! 3. **确认即落地** —— `confirm` 后 EMR 收到契约报文（逐字段 + `Idempotency-Key`）、
//!    企微收到推送、状态落 `written_back`、审计链留痕；
//! 4. **失败不静默改数据** —— EMR 写失败 → 状态如实落 `pending_writeback`、
//!    台账行**保留**、重复确认**不产生第二次回写**。
//!
//! **替身不是最终对接边界**：这里起的是 `127.0.0.1` 上的裸 HTTP 服务，
//! 用来断言**客户端真的按契约发了什么**。院内真实对接边界是 `spec.md` §2.3
//! 那一行（`POST {EMR_BASE_URL}/api/v1/structured-reports` 与
//! `POST {WECOM_WEBHOOK_URL}`），凭据只从环境变量读；真实 EMR / 企微尚未实连
//! （凭据不在本环境），见 `progress.txt`。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ultrasound_report_struct::outbound::emr::EmrClient;
use ultrasound_report_struct::outbound::review_api::{ReviewHandler, ReviewRequest, ReviewServer};
use ultrasound_report_struct::outbound::wecom::WeComClient;
use ultrasound_report_struct::parse::errors::{DetailCode, ErrorCode};
use ultrasound_report_struct::parse::parse_report;
use ultrasound_report_struct::state::idempotency_key;
use ultrasound_report_struct::state::repo::{IngestRecord, NewComparison, NewItem, NewReport, Repo};
use ultrasound_report_struct::workflow::audit::AuditRecorder;

/// 端点令牌（测试用；真实环境从 `DAEMON_API_TOKEN` 读）。
const TOKEN: &str = "review-token-example";
/// EMR 凭据（测试替身用）。
const EMR_TOKEN: &str = "emr-token-example";
/// 确认人账号。
const REVIEWER: &str = "USR-SONO-EXAMPLE";
/// 一份脱敏泌尿系报告（叙述段是**不允许**外发的东西）。
const REPORT: &str = "超声所见：\n左肾前后径 3.2cm\n肾盂轻度扩张\n提示：左肾积水，请结合临床。";

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

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("请求体应是合法 JSON")
    }
}

/// 本地 HTTP 替身（**只在测试数据层**）：同时扮演 EMR 与企业微信群机器人。
struct Stub {
    base_url: String,
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
        let thread_recorded = Arc::clone(&recorded);
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let rec = Arc::clone(&thread_recorded);
                        thread::spawn(move || serve(stream, emr_status, rec));
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
            recorded,
            stop,
            handle: Some(handle),
        }
    }

    /// 收到的全部请求。
    fn requests(&self) -> Vec<Recorded> {
        self.recorded.lock().expect("取替身记录").clone()
    }

    /// 收到的 EMR 写请求（`POST /api/v1/structured-reports`）。
    fn emr_writes(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.target.starts_with("/api/v1/structured-reports"))
            .collect()
    }

    /// 收到的企微推送（`POST /cgi-bin/webhook/send`）。
    fn wecom_pushes(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.target.contains("/cgi-bin/webhook/send"))
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

/// 处理一条连接：读完请求行、请求头与请求体，记下来，再按路径应答。
fn serve(mut stream: TcpStream, emr_status: u16, recorded: Arc<Mutex<Vec<Recorded>>>) {
    stream.set_nonblocking(false).ok();
    let mut reader = BufReader::new(stream.try_clone().expect("克隆连接"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = line.trim_end();
                if text.is_empty() {
                    break;
                }
                if let Some((key, value)) = text.split_once(':') {
                    let key = key.trim().to_string();
                    let value = value.trim().to_string();
                    if key.eq_ignore_ascii_case("content-length") {
                        content_length = value.parse().unwrap_or(0);
                    }
                    headers.push((key, value));
                }
            }
        }
    }
    let mut body_bytes = vec![0u8; content_length];
    if content_length > 0 {
        let mut read = 0usize;
        while read < content_length {
            match reader.read(&mut body_bytes[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(_) => break,
            }
        }
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
        (
            emr_status,
            if emr_status == 200 {
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
    let dir = std::env::temp_dir().join(format!("urs-review-{}-{}", tag, std::process::id()));
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
    fn path(&self) -> String {
        format!("/api/v1/reports/{}/review", self.study_no)
    }

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

/// 处理器（出站指向本地替身）。
fn handler(fixture: &Fixture, stub: &Stub) -> ReviewHandler {
    let emr = EmrClient::new(&stub.base_url, EMR_TOKEN, Duration::from_secs(3)).unwrap();
    let wecom = WeComClient::new(
        &format!("{}/cgi-bin/webhook/send?key=DEMO-KEY-NOT-REAL", stub.base_url),
        Duration::from_secs(3),
    )
    .unwrap();
    ReviewHandler::new(
        fixture.db.clone(),
        TOKEN.to_string(),
        Some(emr),
        Some(wecom),
    )
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

/// 驳回回执体。
fn reject_body() -> String {
    serde_json::json!({
        "decision": "reject",
        "reviewer": REVIEWER,
        "note": "侧别写错了"
    })
    .to_string()
}

#[test]
fn 缺令牌与错令牌都被拒且一条写请求都不发() {
    let fixture = seed("auth", "S-REVIEW-AUTH");
    let stub = Stub::start(200);
    let handler = handler(&fixture, &stub);

    // 缺 Authorization 头。
    let response = handler.handle(&ReviewRequest::new(
        "POST",
        &fixture.path(),
        None,
        &confirm_body(),
    ));
    assert_eq!(response.status, 401);
    assert_eq!(response.error_code(), Some(ErrorCode::ReviewUnauthorized));
    assert_eq!(response.error_detail(), Some(DetailCode::ReviewTokenMissing));

    // 令牌不对。
    let response = handler.handle(&ReviewRequest::new(
        "POST",
        &fixture.path(),
        Some("wrong-token"),
        &confirm_body(),
    ));
    assert_eq!(response.status, 401);
    assert_eq!(response.error_code(), Some(ErrorCode::ReviewUnauthorized));
    assert_eq!(response.error_detail(), Some(DetailCode::ReviewTokenInvalid));

    // 报告状态没被动过 —— 被拒的回执**不改业务状态**。
    assert_eq!(fixture.status(), "parsed");
    // 一条写请求都没发出去。
    assert_eq!(stub.emr_writes().len(), 0);
    assert_eq!(stub.wecom_pushes().len(), 0);
}

#[test]
fn 没有确认就不发出任何写请求() {
    let fixture = seed("noreview", "S-REVIEW-NONE");
    let stub = Stub::start(200);
    let _handler = handler(&fixture, &stub);

    // 台账行落了、差值也落了 —— 但**没人确认，就一条写请求都不该有**。
    let repo = fixture.repo();
    let items: i64 = repo
        .connection()
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(items, 1, "台账行应已落库");
    assert_eq!(fixture.status(), "parsed", "没确认就不该推进状态");
    assert_eq!(stub.emr_writes().len(), 0);
    assert_eq!(stub.wecom_pushes().len(), 0);
}

#[test]
fn 确认后回写_emr_并落_written_back() {
    let fixture = seed("confirm", "S-REVIEW-CONFIRM");
    let stub = Stub::start(200);
    let handler = handler(&fixture, &stub);

    let response = handler.handle(&ReviewRequest::new(
        "POST",
        &fixture.path(),
        Some(TOKEN),
        &confirm_body(),
    ));
    assert_eq!(response.status, 200, "响应体：{}", response.body);
    let value = response.value().expect("响应体应是 JSON");
    assert_eq!(value["status"], "written_back");
    assert_eq!(value["emr_written"], true);
    assert_eq!(value["item_count"], 1);
    assert_eq!(value["comparison_count"], 1);
    assert_eq!(value["reviewer"], REVIEWER);

    // EMR 写请求：逐字段对得上契约。
    let writes = stub.emr_writes();
    assert_eq!(writes.len(), 1, "确认后应恰好发一次 EMR 写请求");
    assert_eq!(writes[0].method, "POST");
    assert_eq!(writes[0].target, "/api/v1/structured-reports");
    assert_eq!(
        writes[0].header("Authorization"),
        Some(format!("Bearer {}", EMR_TOKEN).as_str())
    );
    assert_eq!(
        writes[0].header("Idempotency-Key"),
        Some(idempotency_key(&fixture.study_no, REPORT).as_str())
    );
    let payload = writes[0].json();
    assert_eq!(payload["study_no"], "S-REVIEW-CONFIRM");
    assert_eq!(payload["items"][0]["part_key"], "kidney.left");
    assert_eq!(payload["items"][0]["item_code"], "kidney.ap_diameter");
    assert_eq!(payload["items"][0]["value"], 32.0);
    assert_eq!(payload["items"][0]["unit_code"], "mm");
    assert_eq!(payload["items"][0]["coercion_path"], "UnitConvert");
    assert_eq!(payload["comparisons"][0]["previous_value"], 28.0);
    assert_eq!(payload["comparisons"][0]["delta"], 4.0);
    // 只发结构化结论：报告里的叙述片段一段都不许出现在报文里。
    let body = writes[0].body.clone();
    for fragment in ["超声所见", "肾盂轻度扩张", "请结合临床", "左肾积水"] {
        assert!(
            !body.contains(fragment),
            "外发报文里混进了原文叙述「{}」：{}",
            fragment,
            body
        );
    }

    // 企微推送：一条，内容由结构化字段装配。
    let pushes = stub.wecom_pushes();
    assert_eq!(pushes.len(), 1);
    let message = pushes[0].json();
    assert_eq!(message["msgtype"], "text");
    let content = message["text"]["content"].as_str().unwrap();
    assert!(content.contains("S-REVIEW-CONFIRM"));
    assert!(!content.contains("左肾积水"), "推送正文不得含原文叙述");

    // 状态库：落终态 written_back。
    assert_eq!(fixture.status(), "written_back");

    // 审计链：确认 / 回写 / 推送三类动作都在，且确认人是人工账号。
    let repo = fixture.repo();
    let report = repo.find_by_study_no(&fixture.study_no).unwrap().unwrap();
    let chain = AuditRecorder::new(&repo).chain(report.id).unwrap();
    let actions: Vec<&str> = chain.iter().map(|row| row.action.as_str()).collect();
    assert!(actions.contains(&"confirmed"), "审计链：{:?}", actions);
    assert!(actions.contains(&"written_back"), "审计链：{:?}", actions);
    assert!(actions.contains(&"pushed"), "审计链：{:?}", actions);
    let confirmed = chain
        .iter()
        .find(|row| row.action == "confirmed")
        .expect("应有确认留痕");
    assert_eq!(confirmed.actor.as_deref(), Some(REVIEWER));
    assert!(confirmed.input_digest.is_some(), "确认留痕要带输入摘要哈希");
}

#[test]
fn 驳回落_rejected_且不回写不推送() {
    let fixture = seed("reject", "S-REVIEW-REJECT");
    let stub = Stub::start(200);
    let handler = handler(&fixture, &stub);

    let response = handler.handle(&ReviewRequest::new(
        "POST",
        &fixture.path(),
        Some(TOKEN),
        &reject_body(),
    ));
    assert_eq!(response.status, 200, "响应体：{}", response.body);
    let value = response.value().expect("响应体应是 JSON");
    assert_eq!(value["status"], "rejected");
    assert_eq!(value["emr_written"], false);

    // 驳回**不回写、不推送**。
    assert_eq!(stub.emr_writes().len(), 0);
    assert_eq!(stub.wecom_pushes().len(), 0);
    assert_eq!(fixture.status(), "rejected");

    let repo = fixture.repo();
    let report = repo.find_by_study_no(&fixture.study_no).unwrap().unwrap();
    let chain = AuditRecorder::new(&repo).chain(report.id).unwrap();
    let actions: Vec<&str> = chain.iter().map(|row| row.action.as_str()).collect();
    assert!(actions.contains(&"rejected"), "审计链：{:?}", actions);
    assert!(!actions.contains(&"written_back"), "审计链：{:?}", actions);
}

#[test]
fn 重复确认不产生第二次回写() {
    let fixture = seed("dup", "S-REVIEW-DUP");
    let stub = Stub::start(200);
    let handler = handler(&fixture, &stub);

    let first = handler.handle(&ReviewRequest::new(
        "POST",
        &fixture.path(),
        Some(TOKEN),
        &confirm_body(),
    ));
    assert_eq!(first.status, 200);
    assert_eq!(stub.emr_writes().len(), 1);

    // 第二次确认：报告已是终态 → 显式拒绝，**不再发写请求**。
    let second = handler.handle(&ReviewRequest::new(
        "POST",
        &fixture.path(),
        Some(TOKEN),
        &confirm_body(),
    ));
    assert_eq!(second.status, 409, "响应体：{}", second.body);
    assert_eq!(second.error_code(), Some(ErrorCode::ReviewIllegalState));
    assert_eq!(stub.emr_writes().len(), 1, "不得产生第二次回写");
    assert_eq!(stub.wecom_pushes().len(), 1, "不得产生第二次推送");
}

#[test]
fn 回写失败状态如实落_pending_writeback_且台账行保留() {
    let fixture = seed("emrfail", "S-REVIEW-EMRFAIL");
    let stub = Stub::start(500);
    let handler = handler(&fixture, &stub);

    let response = handler.handle(&ReviewRequest::new(
        "POST",
        &fixture.path(),
        Some(TOKEN),
        &confirm_body(),
    ));
    assert_eq!(response.status, 502, "响应体：{}", response.body);
    assert_eq!(
        response.error_code(),
        Some(ErrorCode::EmrUpstreamUnavailable)
    );

    // 状态如实：转 `pending_writeback`（等重试 / 人工介入），不是「已回写」。
    assert_eq!(fixture.status(), "pending_writeback");
    // 台账行**保留** —— 结论没丢，丢的是回写这一步。
    let repo = fixture.repo();
    let items: i64 = repo
        .connection()
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(items, 1, "台账行不该丢");
    // 失败留痕带稳定错误码。
    let report = repo.find_by_study_no(&fixture.study_no).unwrap().unwrap();
    let chain = AuditRecorder::new(&repo).chain(report.id).unwrap();
    let failure = chain
        .iter()
        .find(|row| row.action == "failed")
        .expect("应有失败留痕");
    assert_eq!(failure.error_code.as_deref(), Some("EMR_UPSTREAM_UNAVAILABLE"));
}

#[test]
fn 没有可回写结论时拒绝回写空报告() {
    let dir = temp_dir("noitem");
    let db = dir.join("state.sqlite");
    let repo = Repo::open(&db).unwrap();
    // 手工造一条**键被误配成中文词**的台账行 —— 装配时会把它挡下来。
    let key = idempotency_key("S-REVIEW-NOITEM", REPORT);
    let report_id = repo
        .insert_report(&NewReport {
            study_no: "S-REVIEW-NOITEM".to_string(),
            patient_id: None,
            status: "parsed".to_string(),
            source_path: None,
            dictionary_version: ultrasound_report_struct::dictionary_version(),
            idempotency_key: key,
        })
        .unwrap();
    repo.insert_item(&NewItem {
        report_id,
        part_key: Some("左肾积水".to_string()),
        item_code: Some("积水深度".to_string()),
        value_raw: Some(3.2),
        unit_raw: Some("cm".to_string()),
        value_si: Some(3.2),
        unit_code: Some("cm".to_string()),
        coercion_path: "UnitConvert".to_string(),
        line: 1,
        column: 1,
        char_start: 0,
        notes: String::new(),
        dictionary_version: ultrasound_report_struct::dictionary_version(),
    })
    .unwrap();
    drop(repo);

    let fixture = Fixture {
        db,
        study_no: "S-REVIEW-NOITEM".to_string(),
    };
    let stub = Stub::start(200);
    let handler = handler(&fixture, &stub);

    let response = handler.handle(&ReviewRequest::new(
        "POST",
        &fixture.path(),
        Some(TOKEN),
        &confirm_body(),
    ));
    assert_eq!(response.status, 409, "响应体：{}", response.body);
    assert_eq!(response.error_code(), Some(ErrorCode::ReviewIllegalState));
    assert_eq!(response.error_detail(), Some(DetailCode::ReviewNoItem));
    // 一条结论都没有 → 绝不回写空报告。
    assert_eq!(stub.emr_writes().len(), 0);
}

#[test]
fn 真实_http_端点可访问() {
    let fixture = seed("socket", "S-REVIEW-SOCKET");
    let stub = Stub::start(200);
    let handler = handler(&fixture, &stub);

    // 绑一个操作系统挑的空闲端口，起**真的** HTTP 端点。
    let server = ReviewServer::bind("127.0.0.1:0", handler).expect("绑定回执端点");
    let addr = server.local_addr().expect("取回执端点地址");
    let mut handle = server.spawn();

    let body = confirm_body();
    let mut stream = TcpStream::connect(addr).expect("连回执端点");
    let request = format!(
        "POST /api/v1/reports/{}/review HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        fixture.study_no,
        addr,
        TOKEN,
        body.len(),
        body
    );
    stream.write_all(request.as_bytes()).expect("发回执请求");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("读回应");
    handle.stop();

    assert!(
        raw.starts_with("HTTP/1.1 200 OK"),
        "真实端点应回 200：{}",
        raw
    );
    assert!(raw.contains("written_back"), "响应体：{}", raw);
    // 真实往返之后，EMR 那边确实收到了一次写请求。
    assert_eq!(stub.emr_writes().len(), 1);
    assert_eq!(fixture.status(), "written_back");
}
