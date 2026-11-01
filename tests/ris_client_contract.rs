//! RIS 出站读**契约测试** —— 真实 HTTP 报文，本地替身只作测试数据层。
//!
//! **替身不是最终对接边界**：这里起的是一个 `127.0.0.1` 上的裸 HTTP 服务，
//! 用来断言**客户端真的按契约发了什么**（方法 / 路径 / 查询参数 / Bearer 头 / 超时行为）。
//! 院内真实对接边界是 `spec.md` §2.3 那一行 ——
//! `GET {RIS_BASE_URL}/api/v1/ultrasound-reports?...` + `Authorization: Bearer $RIS_API_TOKEN`
//! 走 HTTPS 到院内 RIS；替身**不模拟** TLS、不模拟 RIS 的真实业务数据，
//! 真实 RIS 尚未实连（凭据不在本环境），见 `progress.txt`。
//!
//! 四组断言：
//!
//! 1. **请求形状** —— 方法、路径、查询参数、Bearer 头逐项对得上；
//! 2. **响应形状** —— 按契约解析出历史报告（数组与单对象两种顶层形状）；
//! 3. **错误四分类** —— 401 / 5xx / 非 JSON / 超时各落到各自的语义与稳定码；
//! 4. **退避重试** —— 只给「上游不可达」重试，且**最多 2 次**。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ultrasound_report_struct::compare::ris_client::{
    RisClient, RisError, RisErrorKind, RisQuery, DEFAULT_MAX_RETRIES, RIS_HISTORY_PATH,
};
use ultrasound_report_struct::parse::errors::{DetailCode, ErrorCode};

/// 替身的一次应答。
#[derive(Clone, Debug)]
enum Reply {
    /// 回一个 HTTP 响应。
    Respond { status: u16, body: String },
    /// 收下连接但**不回话** —— 用来触发客户端超时。
    Hang,
}

/// 替身收到的一次请求。
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
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

/// 处理一条连接：读完请求行与请求头，记下来，再按脚本应答。
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
                    headers.push((key.trim().to_string(), value.trim().to_string()));
                }
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
                401 => "Unauthorized",
                403 => "Forbidden",
                404 => "Not Found",
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

/// 替身回一份历史报告集合。
fn history_body() -> String {
    r#"[
      {"study_no":"S-HIST-001","patient_id":"PID-EXAMPLE","body_part":"kidney.left",
       "performed_at":"2026-02-10","measurements":[{"item_code":"kidney.ap_diameter","value":28,"unit":"mm"}]},
      {"study_no":"S-HIST-002","patient_id":"PID-EXAMPLE","body_part":"kidney.left",
       "performed_at":"2026-01-05","measurements":[{"item_code":"kidney.ap_diameter","value":"25.5","unit":"mm"}]}
    ]"#
    .to_string()
}

fn ok_reply() -> Reply {
    Reply::Respond {
        status: 200,
        body: history_body(),
    }
}

#[test]
fn 契约_请求打到基准路径且带令牌头与三个查询参数() {
    let stub = Stub::start(vec![ok_reply()]);
    let client = RisClient::new(&stub.base_url, "token-abc123", Duration::from_secs(5)).unwrap();

    let query = RisQuery::for_patient("PID-EXAMPLE")
        .with_body_part("kidney.left")
        .with_since("2026-01-01");
    let reports = client.fetch_history(&query).expect("取历史应成功");
    assert_eq!(reports.len(), 2);

    let requests = stub.requests();
    assert_eq!(requests.len(), 1, "只应发一次请求");
    let req = &requests[0];

    // 方法与路径。
    assert_eq!(req.method, "GET");
    assert!(
        req.target.starts_with(RIS_HISTORY_PATH),
        "请求路径应命中基准路径 {}，实际 {}",
        RIS_HISTORY_PATH,
        req.target
    );

    // 查询参数：三个都在，且值原样带上。
    assert!(req.target.contains("patient_id=PID-EXAMPLE"), "{}", req.target);
    assert!(req.target.contains("body_part=kidney.left"), "{}", req.target);
    assert!(req.target.contains("since=2026-01-01"), "{}", req.target);

    // 凭据：Bearer 头来自环境变量位的令牌（这里由测试显式传入）。
    assert_eq!(req.header("authorization"), Some("Bearer token-abc123"));
    assert_eq!(req.header("accept"), Some("application/json"));
}

#[test]
fn 契约_缺省查询参数时只发患者主索引() {
    let stub = Stub::start(vec![ok_reply()]);
    let client = RisClient::new(&stub.base_url, "t", Duration::from_secs(5)).unwrap();
    client
        .fetch_history(&RisQuery::for_patient("PID-EXAMPLE"))
        .expect("取历史应成功");

    let requests = stub.requests();
    let target = &requests[0].target;
    assert!(target.contains("patient_id=PID-EXAMPLE"), "{}", target);
    assert!(!target.contains("body_part="), "没给部位就不该发部位参数：{}", target);
    assert!(!target.contains("since="), "没给时间就不该发时间参数：{}", target);
}

#[test]
fn 契约_查询参数按网址编码不破坏请求() {
    let stub = Stub::start(vec![ok_reply()]);
    let client = RisClient::new(&stub.base_url, "t", Duration::from_secs(5)).unwrap();
    client
        .fetch_history(&RisQuery::for_patient("PID/001 井号#1"))
        .expect("取历史应成功");

    let requests = stub.requests();
    let target = &requests[0].target;
    // 特殊字符必须编码，不能把请求串拆坏。
    assert!(!target.contains(' '), "空格必须编码：{}", target);
    assert!(!target.contains('#'), "井号必须编码：{}", target);
    assert!(target.contains("patient_id=PID%2F001"), "{}", target);
}

#[test]
fn 契约_响应按契约解析出历史报告() {
    let stub = Stub::start(vec![ok_reply()]);
    let client = RisClient::new(&stub.base_url, "t", Duration::from_secs(5)).unwrap();
    let reports = client
        .fetch_history(&RisQuery::for_patient("PID-EXAMPLE"))
        .expect("取历史应成功");

    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].study_no, "S-HIST-001");
    assert_eq!(reports[0].body_part.as_deref(), Some("kidney.left"));
    assert_eq!(reports[0].measurements.len(), 1);
    assert_eq!(reports[0].measurements[0].item_code, "kidney.ap_diameter");
    assert_eq!(reports[0].measurements[0].value, Some(28.0));
    assert_eq!(reports[0].measurements[0].unit.as_deref(), Some("mm"));
    // 数值给成字符串也照样解出（院内 RIS 两种写法都见过）。
    assert_eq!(reports[1].measurements[0].value, Some(25.5));
}

#[test]
fn 契约_单对象响应归一成一份历史() {
    let body = r#"{"study_no":"S-HIST-ONLY","patient_id":"PID-EXAMPLE",
                    "measurements":[{"item_code":"kidney.ap_diameter","value":28,"unit":"mm"}]}"#;
    let stub = Stub::start(vec![Reply::Respond {
        status: 200,
        body: body.to_string(),
    }]);
    let client = RisClient::new(&stub.base_url, "t", Duration::from_secs(5)).unwrap();
    let reports = client
        .fetch_history(&RisQuery::for_patient("PID-EXAMPLE"))
        .expect("取历史应成功");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].study_no, "S-HIST-ONLY");
}

#[test]
fn 契约_401判鉴权失败且不重试() {
    let stub = Stub::start(vec![Reply::Respond {
        status: 401,
        body: "{\"error\":\"令牌无效\"}".to_string(),
    }]);
    let client = RisClient::new(&stub.base_url, "bad-token", Duration::from_secs(5)).unwrap();
    let err = client
        .fetch_history(&RisQuery::for_patient("PID-EXAMPLE"))
        .expect_err("401 应判失败");

    assert_eq!(err.kind, RisErrorKind::AuthFailed);
    assert_eq!(err.code(), "RIS_AUTH_FAILED");
    assert_eq!(err.detail, Some(DetailCode::RisUnauthorized));
    assert!(!err.retryable(), "鉴权失败重试没意义");
    assert_eq!(err.attempts, 1, "鉴权失败不应重试");
    assert_eq!(stub.requests().len(), 1, "只应发一次请求");
}

#[test]
fn 契约_500判上游不可达并退避重试两次后放弃() {
    let stub = Stub::start(vec![
        Reply::Respond {
            status: 503,
            body: "{\"error\":\"上游忙\"}".to_string(),
        },
        Reply::Respond {
            status: 503,
            body: "{\"error\":\"上游忙\"}".to_string(),
        },
        Reply::Respond {
            status: 503,
            body: "{\"error\":\"上游忙\"}".to_string(),
        },
    ]);
    let client = RisClient::new(&stub.base_url, "t", Duration::from_secs(5))
        .unwrap()
        .with_backoff(Duration::from_millis(10));
    let err = client
        .fetch_history(&RisQuery::for_patient("PID-EXAMPLE"))
        .expect_err("5xx 应判失败");

    assert_eq!(err.kind, RisErrorKind::UpstreamUnavailable);
    assert_eq!(err.code(), "RIS_UPSTREAM_UNAVAILABLE");
    assert_eq!(err.detail, Some(DetailCode::RisServerError));
    assert!(err.retryable());
    // 退避重试 ≤ 2 次 → 最多 3 次尝试。
    assert_eq!(err.attempts, DEFAULT_MAX_RETRIES + 1);
    assert_eq!(stub.requests().len(), DEFAULT_MAX_RETRIES + 1);
}

#[test]
fn 契约_非结构化响应判坏响应且不重试() {
    let stub = Stub::start(vec![Reply::Respond {
        status: 200,
        body: "<html><body>502 Bad Gateway</body></html>".to_string(),
    }]);
    let client = RisClient::new(&stub.base_url, "t", Duration::from_secs(5)).unwrap();
    let err = client
        .fetch_history(&RisQuery::for_patient("PID-EXAMPLE"))
        .expect_err("非 JSON 应判失败");

    assert_eq!(err.kind, RisErrorKind::BadResponse);
    assert_eq!(err.code(), "RIS_BAD_RESPONSE");
    assert_eq!(err.detail, Some(DetailCode::RisBodyNotJson));
    assert_eq!(err.attempts, 1, "响应不合契约重试也只会再错一次");
    assert_eq!(stub.requests().len(), 1);
}

#[test]
fn 契约_字段不合契约判坏响应() {
    let stub = Stub::start(vec![Reply::Respond {
        status: 200,
        // 缺 study_no —— 没有它认不出这是哪份报告。
        body: "[{\"patient_id\":\"PID-EXAMPLE\",\"measurements\":[]}]".to_string(),
    }]);
    let client = RisClient::new(&stub.base_url, "t", Duration::from_secs(5)).unwrap();
    let err = client
        .fetch_history(&RisQuery::for_patient("PID-EXAMPLE"))
        .expect_err("字段不合契约应判失败");
    assert_eq!(err.kind, RisErrorKind::BadResponse);
    assert_eq!(err.detail, Some(DetailCode::RisBodySchemaInvalid));
}

#[test]
fn 契约_上游不回话时在超时上限内失败不挂死() {
    let stub = Stub::start(vec![Reply::Hang, Reply::Hang, Reply::Hang]);
    let timeout = Duration::from_millis(400);
    let client = RisClient::new(&stub.base_url, "t", timeout)
        .unwrap()
        .with_backoff(Duration::from_millis(10));

    let started = Instant::now();
    let err = client
        .fetch_history(&RisQuery::for_patient("PID-EXAMPLE"))
        .expect_err("不回话应当超时失败");
    let elapsed = started.elapsed();

    // 失败语义：上游不可达（**绝不当作「无差异」**）。
    assert_eq!(err.kind, RisErrorKind::UpstreamUnavailable);
    assert_eq!(err.code(), "RIS_UPSTREAM_UNAVAILABLE");
    // 超时与连接失败在运维眼里是两回事 —— 归到超时这一类。
    assert_eq!(err.detail, Some(DetailCode::RisTimeout));
    // 挂了也得在超时上限的量级内返回（3 次尝试 × 超时 + 退避）。
    assert!(
        elapsed < timeout * 6,
        "应在超时上限内失败，实际耗时 {} 毫秒",
        elapsed.as_millis()
    );
    // 退避重试 ≤ 2 次。
    assert_eq!(err.attempts, DEFAULT_MAX_RETRIES + 1);
}

#[test]
fn 契约_端点或凭据未配置时显式失败不发请求() {
    // 端点缺失。
    let err = RisClient::new("", "token", Duration::from_secs(1)).expect_err("缺端点应失败");
    assert_eq!(err.kind, RisErrorKind::NotConfigured);
    assert_eq!(err.code(), "RIS_NOT_CONFIGURED");
    assert_eq!(err.error_code(), ErrorCode::RisNotConfigured);

    // 凭据缺失。
    let err = RisClient::new("https://ris.intranet", "", Duration::from_secs(1)).expect_err("缺令牌应失败");
    assert_eq!(err.kind, RisErrorKind::NotConfigured);
    assert_eq!(err.code(), "RIS_NOT_CONFIGURED");
}

#[test]
fn 契约_连不上真实端点判上游不可达() {
    // 指向一个没人在听的端口（先占一个端口再放掉）。
    let listener = TcpListener::bind("127.0.0.1:0").expect("占端口");
    let addr = listener.local_addr().expect("取端口");
    drop(listener);

    let client = RisClient::new(&format!("http://{}", addr), "t", Duration::from_millis(500))
        .unwrap()
        .with_backoff(Duration::from_millis(10));
    let err = client
        .fetch_history(&RisQuery::for_patient("PID-EXAMPLE"))
        .expect_err("连不上应判失败");
    assert_eq!(err.kind, RisErrorKind::UpstreamUnavailable);
    assert_eq!(err.code(), "RIS_UPSTREAM_UNAVAILABLE");
    assert!(err.retryable());
}

#[test]
fn 契约_失败条目带共用稳定码且不编造位置() {
    let err = RisError::not_configured("RIS_BASE_URL 未设置（出站读端点缺失）");
    let failure = err.to_failure();
    assert_eq!(failure.code, ErrorCode::RisNotConfigured);
    // 出站失败不在报告正文里 —— 行列一律 0，不编造。
    assert_eq!(failure.line, 0);
    assert_eq!(failure.column, 0);
    assert_eq!(failure.char_start, 0);
    // 处置动作是「待上游恢复」，不是「整份退回」—— 报告本身没问题。
    assert!(!failure.policy().blocks_report);
    assert!(failure.policy().excluded_from_comparison);
}
