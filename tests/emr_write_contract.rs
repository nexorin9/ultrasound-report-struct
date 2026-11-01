//! EMR 出站写**契约测试** —— 真实 HTTP 报文，本地替身只作测试数据层。
//!
//! **替身不是最终对接边界**：这里起的是一个 `127.0.0.1` 上的裸 HTTP 服务，
//! 用来断言**客户端真的按契约发了什么**（方法 / 路径 / 头 / 请求体逐字段）。
//! 院内真实对接边界是 `spec.md` §2.3 那一行 —— `POST {EMR_BASE_URL}/api/v1/structured-reports`
//! 配 `Authorization: Bearer $EMR_API_TOKEN` 与
//! `Idempotency-Key: sha256(study_no‖报告内容)`，走 HTTPS 到院内 EMR；
//! 替身**不模拟** TLS、**不模拟** EMR 的真实业务数据，
//! 真实 EMR 尚未实连（凭据不在本环境），见 `progress.txt`。
//!
//! 四组断言：
//!
//! 1. **请求形状** —— 方法、路径、三个头（Bearer / Idempotency-Key / Content-Type）；
//! 2. **请求体字段** —— 逐字段对得上 `spec.md` §2.3 的契约，顶层不多不少；
//! 3. **只发结构化结论** —— 出站报文扫描：原始报告的叙述段落**一段都不许出现**；
//! 4. **错误四分类** —— 401 / 5xx / 4xx 各落到各自的语义与稳定码。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use sha2::{Digest, Sha256};
use ultrasound_report_struct::outbound::emr::{
    build_payload, classify_status, EmrClient, EmrErrorKind, EMR_STRUCTURED_REPORT_PATH,
};
use ultrasound_report_struct::parse::errors::{DetailCode, ErrorCode};

/// 一份**带叙述段**的脱敏超声报告（叙述段是不许外发的东西）。
const REPORT: &str = "超声所见：\n左肾前后径 3.2cm\n肾盂轻度扩张\n提示：左肾积水，请结合临床。";

/// 替身的一次应答。
#[derive(Clone, Debug)]
enum Reply {
    /// 回一个 HTTP 响应。
    Respond { status: u16, body: String },
    /// 收下连接但**不回话** —— 用来触发客户端超时。
    Hang,
}

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

    /// 请求体解析成 JSON（请求体是本产品自己组装的，必然是合法 JSON）。
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("请求体应是合法 JSON")
    }
}

/// 本地 HTTP 替身（**只在测试数据层**）。
struct Stub {
    base_url: String,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Stub {
    fn start(replies: Vec<Reply>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定替身端口");
        let addr = listener.local_addr().expect("取替身地址");
        listener.set_nonblocking(true).expect("替身转非阻塞");

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_recorded = Arc::clone(&recorded);
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            let mut index = 0usize;
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let reply = replies.get(index).cloned().unwrap_or(Reply::Respond {
                            status: 503,
                            body: "{\"error\":\"替身没有更多应答了\"}".to_string(),
                        });
                        index += 1;
                        let rec = Arc::clone(&thread_recorded);
                        let stop = Arc::clone(&thread_stop);
                        // 每次连接单独一个线程 —— 重试时不会排在上一条后面。
                        thread::spawn(move || serve(stream, reply, rec, stop));
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

    /// 收到的请求（按到达顺序）。
    fn requests(&self) -> Vec<Recorded> {
        self.recorded.lock().expect("取替身记录").clone()
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

/// 处理一条连接：读完请求行、请求头与请求体，记下来，再按脚本应答。
fn serve(
    mut stream: TcpStream,
    reply: Reply,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
) {
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
    // 请求体按 Content-Length 读满（客户端分片发也要读全）。
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
        target,
        headers,
        body: String::from_utf8_lossy(&body_bytes).to_string(),
    });

    match reply {
        Reply::Hang => {
            // 握住连接不回话，直到替身被关闭 —— 客户端那边应当超时。
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(20));
            }
        }
        Reply::Respond { status, body } => {
            let reason = match status {
                200 => "OK",
                201 => "Created",
                401 => "Unauthorized",
                403 => "Forbidden",
                409 => "Conflict",
                422 => "Unprocessable Entity",
                500 => "Internal Server Error",
                503 => "Service Unavailable",
                _ => "Status",
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
    }
}

/// 独立算一遍 sha256（**不复用产品代码里的算法** —— 复用就等于自己验自己）。
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect()
}

/// 契约算法：幂等键 = sha256(study_no ‖ 报告内容)。
fn expected_idempotency_key(study_no: &str, raw_body: &str) -> String {
    let mut data = Vec::new();
    data.extend_from_slice(study_no.as_bytes());
    data.extend_from_slice(raw_body.as_bytes());
    sha256_hex(&data)
}

fn ok_reply() -> Reply {
    Reply::Respond {
        status: 201,
        body: "{\"id\":\"SR-EXAMPLE-001\",\"status\":\"accepted\"}".to_string(),
    }
}

/// 装配一份真实解析出来的报文（走产品主路径：解析 → 装配）。
fn build_from_report(raw: &str) -> ultrasound_report_struct::outbound::emr::PayloadBuild {
    let outcome = ultrasound_report_struct::parse::parse_report(raw);
    build_payload(
        "SAMPLE-URO-001",
        Some("PID-EXAMPLE"),
        &outcome.items,
        &[],
        &outcome.dictionary_version,
    )
}

#[test]
fn 契约_请求打到基准路径且带令牌头与幂等键() {
    let stub = Stub::start(vec![ok_reply()]);
    let client = EmrClient::new(&stub.base_url, "token-abc123", Duration::from_secs(5)).unwrap();
    let build = build_from_report(REPORT);
    let key = expected_idempotency_key("SAMPLE-URO-001", REPORT);

    let receipt = client.send(&build.payload, &key).expect("回写应成功");
    assert_eq!(receipt.status, 201);
    assert_eq!(receipt.receipt_id.as_deref(), Some("SR-EXAMPLE-001"));

    let requests = stub.requests();
    assert_eq!(requests.len(), 1, "只应发一次请求");
    let req = &requests[0];

    // 方法与路径。
    assert_eq!(req.method, "POST");
    assert!(
        req.target.starts_with(EMR_STRUCTURED_REPORT_PATH),
        "请求路径应命中基准路径 {}，实际 {}",
        EMR_STRUCTURED_REPORT_PATH,
        req.target
    );

    // 凭据：Bearer 头来自环境变量位的令牌。
    assert_eq!(req.header("authorization"), Some("Bearer token-abc123"));
    assert_eq!(req.header("content-type"), Some("application/json"));
    // 幂等键：契约算法 sha256(study_no‖报告内容)，独立算一遍。
    assert_eq!(
        req.header("idempotency-key"),
        Some(key.as_str()),
        "幂等键须等于 sha256(study_no‖报告内容)"
    );
    assert_eq!(req.header("idempotency-key").unwrap().len(), 64);
}

#[test]
fn 契约_请求体逐字段对得上字段契约() {
    let stub = Stub::start(vec![ok_reply()]);
    let client = EmrClient::new(&stub.base_url, "t", Duration::from_secs(5)).unwrap();
    let build = build_from_report(REPORT);
    client
        .send(&build.payload, &expected_idempotency_key("SAMPLE-URO-001", REPORT))
        .expect("回写应成功");

    let req = &stub.requests()[0];
    let body = req.json();

    // 顶层字段：严格按 spec §2.3 —— 不多不少。
    let object = body.as_object().expect("请求体应为 JSON 对象");
    let mut keys: Vec<&str> = object.keys().map(|k| k.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "comparisons",
            "dictionary_version",
            "items",
            "patient_id",
            "study_no"
        ]
    );

    assert_eq!(body["study_no"], "SAMPLE-URO-001");
    assert_eq!(body["patient_id"], "PID-EXAMPLE");
    assert!(body["dictionary_version"].is_string());
    assert!(!body["dictionary_version"].as_str().unwrap().is_empty());

    // 条目字段：部位码 / 测项码 / 数值 / 单位 / 强转路径 / 位置。
    let items = body["items"].as_array().expect("items 应是数组");
    assert_eq!(items.len(), 1, "这份报告应解析出一条测值");
    let item = &items[0];
    assert_eq!(item["part_key"], "kidney.left");
    assert_eq!(item["unit_code"], "mm");
    assert_eq!(item["value"], 32.0);
    assert_eq!(item["coercion_path"], "UnitConvert");
    assert!(item["line"].as_u64().unwrap() >= 1, "位置应带 1 基行号");
    assert!(item["column"].as_u64().unwrap() >= 1, "位置应带 1 基列号");

    // 差值列表：本例没有历史 → 空数组（**不是**省略字段）。
    assert!(body["comparisons"].is_array());
    assert_eq!(body["comparisons"].as_array().unwrap().len(), 0);
}

#[test]
fn 契约_出站报文不含原始报告段落文本() {
    let stub = Stub::start(vec![ok_reply()]);
    let client = EmrClient::new(&stub.base_url, "t", Duration::from_secs(5)).unwrap();
    let build = build_from_report(REPORT);
    client
        .send(&build.payload, &expected_idempotency_key("SAMPLE-URO-001", REPORT))
        .expect("回写应成功");

    // **扫描真正发出去的那份报文**（不是内存里的对象）——
    // 线上出事时看到的就是这一串字节。
    let req = &stub.requests()[0];
    let hits = ultrasound_report_struct::outbound::emr::scan_forbidden_text(&req.body, REPORT);
    assert!(
        hits.is_empty(),
        "外发报文里出现了原始报告叙述片段：{:?}",
        hits
    );

    // 逐条点名：报告里的叙述段一段都不许出现在报文里。
    for fragment in ["超声所见", "肾盂轻度扩张", "请结合临床", "左肾积水"] {
        assert!(
            !req.body.contains(fragment),
            "报文里不该出现「{}」：{}",
            fragment,
            req.body
        );
    }
    // 反过来：结构化结论该在报文里。
    assert!(req.body.contains("kidney.left"));
    assert!(req.body.contains("32"));
    assert!(req.body.contains("mm"));
}

#[test]
fn 契约_401判鉴权失败且不重试() {
    let stub = Stub::start(vec![Reply::Respond {
        status: 401,
        body: "{\"error\":\"令牌无效\"}".to_string(),
    }]);
    let client = EmrClient::new(&stub.base_url, "bad-token", Duration::from_secs(5)).unwrap();
    let build = build_from_report(REPORT);
    let err = client
        .send(
            &build.payload,
            &expected_idempotency_key("SAMPLE-URO-001", REPORT),
        )
        .expect_err("401 应判失败");

    assert_eq!(err.kind, EmrErrorKind::AuthFailed);
    assert_eq!(err.code(), "EMR_AUTH_FAILED");
    assert_eq!(err.error_code(), ErrorCode::EmrAuthFailed);
    assert_eq!(err.detail, Some(DetailCode::EmrUnauthorized));
    assert!(!err.retryable(), "鉴权失败重试没意义");
    assert_eq!(stub.requests().len(), 1, "只应发一次请求");
}

#[test]
fn 契约_5xx判上游不可达() {
    let stub = Stub::start(vec![Reply::Respond {
        status: 503,
        body: "{\"error\":\"上游忙\"}".to_string(),
    }]);
    let client = EmrClient::new(&stub.base_url, "t", Duration::from_secs(5)).unwrap();
    let build = build_from_report(REPORT);
    let err = client
        .send(
            &build.payload,
            &expected_idempotency_key("SAMPLE-URO-001", REPORT),
        )
        .expect_err("5xx 应判失败");

    assert_eq!(err.kind, EmrErrorKind::UpstreamUnavailable);
    assert_eq!(err.code(), "EMR_UPSTREAM_UNAVAILABLE");
    assert_eq!(err.detail, Some(DetailCode::EmrServerError));
    assert!(err.retryable(), "上游不可达才值得退避重试");
}

#[test]
fn 契约_4xx判请求被拒() {
    let stub = Stub::start(vec![Reply::Respond {
        status: 422,
        body: "{\"error\":\"字段不合契约\"}".to_string(),
    }]);
    let client = EmrClient::new(&stub.base_url, "t", Duration::from_secs(5)).unwrap();
    let build = build_from_report(REPORT);
    let err = client
        .send(
            &build.payload,
            &expected_idempotency_key("SAMPLE-URO-001", REPORT),
        )
        .expect_err("4xx 应判失败");

    assert_eq!(err.kind, EmrErrorKind::BadResponse);
    assert_eq!(err.code(), "EMR_BAD_RESPONSE");
    assert_eq!(err.detail, Some(DetailCode::EmrClientError));
    assert!(!err.retryable());
}

#[test]
fn 契约_端点或凭据未配置时显式失败不发请求() {
    // 端点缺失。
    let err = EmrClient::new("", "token", Duration::from_secs(1)).expect_err("缺端点应失败");
    assert_eq!(err.kind, EmrErrorKind::NotConfigured);
    assert_eq!(err.code(), "EMR_NOT_CONFIGURED");
    assert_eq!(err.error_code(), ErrorCode::EmrNotConfigured);

    // 凭据缺失。
    let err =
        EmrClient::new("https://emr.intranet", "", Duration::from_secs(1)).expect_err("缺令牌应失败");
    assert_eq!(err.kind, EmrErrorKind::NotConfigured);
    assert!(err.reason.contains("EMR_API_TOKEN"));
}

#[test]
fn 契约_连不上真实端点判上游不可达() {
    // 指向一个没人在听的端口（先占一个端口再放掉）。
    let listener = TcpListener::bind("127.0.0.1:0").expect("占端口");
    let addr = listener.local_addr().expect("取端口");
    drop(listener);

    let client = EmrClient::new(&format!("http://{}", addr), "t", Duration::from_millis(500))
        .unwrap();
    let build = build_from_report(REPORT);
    let err = client
        .send(
            &build.payload,
            &expected_idempotency_key("SAMPLE-URO-001", REPORT),
        )
        .expect_err("连不上应判失败");
    assert_eq!(err.kind, EmrErrorKind::UpstreamUnavailable);
    assert_eq!(err.code(), "EMR_UPSTREAM_UNAVAILABLE");
}

#[test]
fn 契约_上游不回话时在超时上限内失败不挂死() {
    let stub = Stub::start(vec![Reply::Hang]);
    let client = EmrClient::new(&stub.base_url, "t", Duration::from_millis(400)).unwrap();
    let build = build_from_report(REPORT);

    let started = std::time::Instant::now();
    let err = client
        .send(
            &build.payload,
            &expected_idempotency_key("SAMPLE-URO-001", REPORT),
        )
        .expect_err("不回话应当超时失败");
    let elapsed = started.elapsed();

    assert_eq!(err.kind, EmrErrorKind::UpstreamUnavailable);
    assert_eq!(err.code(), "EMR_UPSTREAM_UNAVAILABLE");
    assert_eq!(err.detail, Some(DetailCode::EmrTimeout));
    assert!(
        elapsed < Duration::from_millis(3000),
        "应在超时上限的量级内返回，实际耗时 {} 毫秒",
        elapsed.as_millis()
    );
}

#[test]
fn 契约_真实出站请求报文可打印核对() {
    // 这一条用例**只负责留下证据**：把真正发到线上的请求（方法 / 路径 / 头 / 报文）
    // 打印出来，便于留档核对 —— 线上出事时看到的就是这一串字节。
    let stub = Stub::start(vec![ok_reply()]);
    let client = EmrClient::new(&stub.base_url, "token-abc123", Duration::from_secs(5)).unwrap();
    let build = build_from_report(REPORT);
    let key = expected_idempotency_key("SAMPLE-URO-001", REPORT);
    let receipt = client.send(&build.payload, &key).expect("回写应成功");

    let req = &stub.requests()[0];
    println!("=== 真实出站请求（EMR 回写）===");
    println!("{} {}", req.method, req.target);
    for (name, value) in &req.headers {
        println!("{}: {}", name, value);
    }
    println!();
    println!("{}", req.body);
    println!("=== 回执 ===");
    println!(
        "status={} receipt_id={} body={}",
        receipt.status,
        receipt.receipt_id.as_deref().unwrap_or("-"),
        receipt.body_snippet
    );
    assert_eq!(receipt.status, 201);
}

#[test]
fn 契约_失败条目带共用稳定码且不编造位置() {
    let err = classify_status(503, "忙");
    let failure = err.to_failure();
    assert_eq!(failure.code, ErrorCode::EmrUpstreamUnavailable);
    assert_eq!(failure.detail_code(), Some(DetailCode::EmrServerError));
    // 出站失败不在报告正文里 —— 行列一律 0，不编造。
    assert_eq!(failure.line, 0);
    assert_eq!(failure.column, 0);
    assert_eq!(failure.char_start, 0);
    // 处置动作是「待回写重试」—— 台账行保留，结论没丢。
    assert_eq!(
        failure.disposition(),
        ultrasound_report_struct::parse::errors::Disposition::AwaitingWriteback
    );
    assert!(!failure.policy().blocks_report);
}
